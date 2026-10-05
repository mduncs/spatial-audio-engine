use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fightbox_api::EnuVector3;
use fightbox_world::{
    AcousticMesh, CapabilityExtension, CellBounds, CellGridIndex, CompileOptions,
    ExtensionRequirement, GeoJsonOptions, GeoJsonProvider, GeodeticOrigin, Material, MaterialTable,
    ObjProvider, Provenance, ProviderGeometry, STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY,
    TriangleProvider, WORLD_MANIFEST_V2_SCHEMA_ID, WORLD_MANIFEST_V2_SCHEMA_JSON, WorldError,
    WorldPackageV2Metadata, compile, export_obj, mesh_content_hash, read_package,
    read_package_with_capabilities, write_package, write_package_v2,
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/city/synthetic")
            .join(name),
    )
    .unwrap()
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fightbox-world-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn geojson_extrudes_prisms_adds_ground_and_uses_levels_fallback() {
    let bytes = fixture("block.geojson");
    let provider = GeoJsonProvider::new(&bytes, GeoJsonOptions::default());
    let geometry = provider.provide().unwrap();
    assert_eq!(geometry.ignored_hole_count, 1);
    assert_eq!(geometry.vertices_enu_m.len(), 50);
    assert_eq!(geometry.triangles.len(), 74);
    assert!(
        geometry.vertices_enu_m[12..16]
            .iter()
            .all(|vertex| vertex.up_m == 16.0)
    );

    let mesh = compile(
        &provider,
        &MaterialTable::default(),
        CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(mesh.triangles.len(), 74);
    assert_eq!(mesh.material_ids.len(), 74);
}

#[test]
fn generated_winding_faces_outward() {
    let json = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"height":3},"geometry":{"type":"Polygon","coordinates":[[[0,0],[4,0],[4,2],[0,2],[0,0]]]}}]}"#;
    let geometry = GeoJsonProvider::new(json, GeoJsonOptions::default())
        .provide()
        .unwrap();
    for triangle in &geometry.triangles[..12] {
        let vertices = triangle.map(|index| geometry.vertices_enu_m[index as usize]);
        let normal = normal(vertices);
        if vertices.iter().all(|vertex| vertex.up_m == 3.0) {
            assert!(normal.up_m > 0.0, "roof must face up");
        } else if vertices.iter().all(|vertex| vertex.up_m == 0.0) {
            assert!(normal.up_m < 0.0, "prism bottom must face down");
        } else {
            let centroid_east =
                vertices.iter().map(|vertex| vertex.east_m).sum::<f32>() / 3.0 - 2.0;
            let centroid_north =
                vertices.iter().map(|vertex| vertex.north_m).sum::<f32>() / 3.0 - 1.0;
            assert!(
                normal.east_m * centroid_east + normal.north_m * centroid_north > 0.0,
                "wall must face away from footprint center"
            );
        }
    }
    for triangle in &geometry.triangles[12..] {
        assert!(normal(triangle.map(|index| geometry.vertices_enu_m[index as usize])).up_m > 0.0);
    }
}

fn normal(vertices: [EnuVector3; 3]) -> EnuVector3 {
    let ab = EnuVector3::new(
        vertices[1].east_m - vertices[0].east_m,
        vertices[1].north_m - vertices[0].north_m,
        vertices[1].up_m - vertices[0].up_m,
    );
    let ac = EnuVector3::new(
        vertices[2].east_m - vertices[0].east_m,
        vertices[2].north_m - vertices[0].north_m,
        vertices[2].up_m - vertices[0].up_m,
    );
    EnuVector3::new(
        ab.north_m * ac.up_m - ab.up_m * ac.north_m,
        ab.up_m * ac.east_m - ab.east_m * ac.up_m,
        ab.east_m * ac.north_m - ab.north_m * ac.east_m,
    )
}

#[test]
fn obj_imports_triangulated_faces_and_rejects_non_triangles() {
    let bytes = fixture("tiny.obj");
    let mesh = compile(
        &ObjProvider::new(&bytes, "concrete"),
        &MaterialTable::default(),
        CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(mesh.vertices_enu_m.len(), 4);
    assert_eq!(mesh.triangles.len(), 4);

    let quad = b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n";
    assert!(matches!(
        ObjProvider::new(quad, "concrete").provide(),
        Err(WorldError::InvalidObj(message)) if message.contains("only triangulated faces")
    ));
}

#[test]
fn exported_obj_round_trips_vertices_triangles_and_material_assignments() {
    let source = fixture("block.geojson");
    let materials = MaterialTable::default();
    let original = compile(
        &GeoJsonProvider::new(&source, GeoJsonOptions::default()),
        &materials,
        CompileOptions::default(),
    )
    .unwrap();
    let obj = export_obj(&original, &materials).unwrap();
    let imported = compile(
        &ObjProvider::new(&obj, "concrete"),
        &materials,
        CompileOptions::default(),
    )
    .unwrap();

    assert_eq!(imported.vertices_enu_m, original.vertices_enu_m);
    assert_eq!(imported.triangles, original.triangles);
    assert_eq!(imported.material_ids, original.material_ids);
}

#[test]
fn rejects_expected_failure_fixtures() {
    let self_intersecting = fixture("self-intersecting.geojson");
    assert!(matches!(
        GeoJsonProvider::new(&self_intersecting, GeoJsonOptions::default()).provide(),
        Err(WorldError::SelfIntersectingPolygon { feature: 0 })
    ));

    let non_finite = fixture("non-finite.obj");
    assert!(matches!(
        compile(
            &ObjProvider::new(&non_finite, "concrete"),
            &MaterialTable::default(),
            CompileOptions::default()
        ),
        Err(WorldError::NonFiniteVertex { vertex: 0 })
    ));

    let unknown = fixture("unknown-material.geojson");
    assert!(matches!(
        compile(
            &GeoJsonProvider::new(&unknown, GeoJsonOptions::default()),
            &MaterialTable::default(),
            CompileOptions::default()
        ),
        Err(WorldError::UnknownMaterial(name)) if name == "unobtainium"
    ));
}

struct StaticProvider(ProviderGeometry);

impl TriangleProvider for StaticProvider {
    fn provide(&self) -> fightbox_world::Result<ProviderGeometry> {
        Ok(self.0.clone())
    }
}

fn provider_with(
    vertices: Vec<EnuVector3>,
    triangles: Vec<[u32; 3]>,
    materials: Vec<&str>,
) -> StaticProvider {
    StaticProvider(ProviderGeometry {
        vertices_enu_m: vertices,
        triangles,
        material_names: materials.into_iter().map(str::to_owned).collect(),
        ignored_hole_count: 0,
        building_count: 0,
        assumptions: Vec::new(),
    })
}

fn bounds_for(mesh: &AcousticMesh) -> CellBounds {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for vertex in &mesh.vertices_enu_m {
        for (axis, value) in [vertex.east_m, vertex.north_m, vertex.up_m]
            .into_iter()
            .map(f64::from)
            .enumerate()
        {
            min[axis] = min[axis].min(value);
            max[axis] = max[axis].max(value);
        }
    }
    CellBounds {
        min_enu_m: min,
        max_enu_m: max,
    }
}

#[test]
fn acoustic_mesh_validation_rejects_each_required_invariant() {
    let vertices = vec![
        EnuVector3::new(0.0, 0.0, 0.0),
        EnuVector3::new(1.0, 0.0, 0.0),
        EnuVector3::new(0.0, 1.0, 0.0),
    ];
    let materials = MaterialTable::default();

    assert!(matches!(
        compile(
            &provider_with(vertices.clone(), vec![[0, 1, 3]], vec!["brick"]),
            &materials,
            CompileOptions::default()
        ),
        Err(WorldError::IndexOutOfRange {
            triangle: 0,
            index: 3
        })
    ));
    assert!(matches!(
        compile(
            &provider_with(vertices.clone(), vec![[0, 1, 1]], vec!["brick"]),
            &materials,
            CompileOptions::default()
        ),
        Err(WorldError::DegenerateTriangle { triangle: 0 })
    ));
    assert!(matches!(
        compile(
            &provider_with(vertices.clone(), vec![[0, 1, 2]], vec![]),
            &materials,
            CompileOptions::default()
        ),
        Err(WorldError::MissingMaterialAssignment { triangle: 0 })
    ));
    assert!(matches!(
        compile(
            &provider_with(vertices, vec![[0, 1, 2]], vec!["brick"]),
            &materials,
            CompileOptions { triangle_budget: 0 }
        ),
        Err(WorldError::TriangleBudgetExceeded {
            actual: 1,
            budget: 0
        })
    ));
}

#[test]
fn material_table_is_named_sorted_and_validated() {
    let table = MaterialTable::default();
    assert_eq!(
        table.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        vec!["asphalt", "brick", "concrete", "glass", "grass"]
    );
    assert_eq!(table.id("asphalt").unwrap(), 0);
    assert_eq!(table.id("grass").unwrap(), 4);

    let invalid = Material {
        absorption: [0.0, 1.1, 0.0],
        scattering: 0.0,
        transmission: [0.0; 3],
    };
    let table = MaterialTable::new([("bad".to_owned(), invalid)].into_iter().collect());
    assert!(matches!(
        table.validate(),
        Err(WorldError::InvalidMaterial { name, .. }) if name == "bad"
    ));
}

#[test]
fn geojson_rejects_missing_height_and_invalid_ground_margin() {
    let missing_height = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[0,1],[0,0]]]}}]}"#;
    assert!(matches!(
        GeoJsonProvider::new(missing_height, GeoJsonOptions::default()).provide(),
        Err(WorldError::InvalidGeoJson(message)) if message.contains("height")
    ));
    let valid = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"height":1},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[0,1],[0,0]]]}}]}"#;
    let options = GeoJsonOptions {
        ground_margin_m: -1.0,
        ..GeoJsonOptions::default()
    };
    assert!(matches!(
        GeoJsonProvider::new(valid, options).provide(),
        Err(WorldError::InvalidGeoJson(message)) if message.contains("ground margin")
    ));
}

#[test]
fn package_round_trip_is_hash_identical_in_fresh_scope_and_deterministic() {
    let bytes = fixture("block.geojson");
    let materials = MaterialTable::default();
    let mesh = compile(
        &GeoJsonProvider::new(&bytes, GeoJsonOptions::default()),
        &materials,
        CompileOptions::default(),
    )
    .unwrap();
    let expected_hash = mesh_content_hash(&mesh).unwrap();
    let provenance = [Provenance::from_bytes("block.geojson", &bytes)];
    let first = TestDirectory::new("package-first");
    let second = TestDirectory::new("package-second");
    write_package(&first.0, &mesh, &materials, &provenance, "test-tool-1").unwrap();
    write_package(&second.0, &mesh, &materials, &provenance, "test-tool-1").unwrap();

    for name in ["manifest.json", "mesh.bin", "materials.json"] {
        assert_eq!(
            fs::read(first.0.join(name)).unwrap(),
            fs::read(second.0.join(name)).unwrap()
        );
    }

    fn fresh_load(path: &Path) -> fightbox_world::LoadedPackage {
        read_package(path).unwrap()
    }
    let loaded = fresh_load(&first.0);
    assert_eq!(loaded.manifest.format_version, 1);
    assert!(loaded.manifest.schema_version.is_none());
    assert!(loaded.manifest.world.is_none());
    assert!(loaded.manifest.extensions.is_empty());
    assert!(loaded.unsupported_optional_capabilities.is_empty());
    assert_eq!(loaded.manifest.mesh_content_sha256, expected_hash);
    assert_eq!(mesh_content_hash(&loaded.mesh).unwrap(), expected_hash);
    assert_eq!(loaded.mesh, mesh);
    assert_eq!(loaded.materials, materials);
}

#[test]
fn package_v2_is_deterministic_round_trips_and_reuses_v1_core_payloads() {
    let bytes = fixture("block.geojson");
    let materials = MaterialTable::default();
    let mesh = compile(
        &GeoJsonProvider::new(&bytes, GeoJsonOptions::default()),
        &materials,
        CompileOptions::default(),
    )
    .unwrap();
    let provenance = [Provenance::from_bytes("block.geojson", &bytes)];
    let grid_index = CellGridIndex { east: 2, north: -1 };
    let world = WorldPackageV2Metadata::mobile_cell(
        "chicago-loop",
        GeodeticOrigin::wgs84(41.881_832, -87.623_177, 181.0),
        grid_index,
        bounds_for(&mesh),
    );
    let first = TestDirectory::new("package-v2-first");
    let second = TestDirectory::new("package-v2-second");
    let legacy = TestDirectory::new("package-v2-legacy-core");
    write_package_v2(
        &first.0,
        &mesh,
        &materials,
        &provenance,
        "test-tool-2",
        &world,
        &[],
    )
    .unwrap();
    write_package_v2(
        &second.0,
        &mesh,
        &materials,
        &provenance,
        "test-tool-2",
        &world,
        &[],
    )
    .unwrap();
    write_package(&legacy.0, &mesh, &materials, &provenance, "test-tool-2").unwrap();

    for name in ["manifest.json", "mesh.bin", "materials.json"] {
        assert_eq!(
            fs::read(first.0.join(name)).unwrap(),
            fs::read(second.0.join(name)).unwrap()
        );
    }
    for name in ["mesh.bin", "materials.json"] {
        assert_eq!(
            fs::read(first.0.join(name)).unwrap(),
            fs::read(legacy.0.join(name)).unwrap(),
            "v2 must preserve the frozen v1 {name} encoding"
        );
    }

    let schema: serde_json::Value = serde_json::from_str(WORLD_MANIFEST_V2_SCHEMA_JSON).unwrap();
    assert_eq!(
        schema["$id"],
        "https://fightbox.dev/schema/world-manifest-2.json"
    );
    assert_eq!(
        schema["properties"]["schema_version"]["const"],
        WORLD_MANIFEST_V2_SCHEMA_ID
    );
    let loaded = read_package(&first.0).unwrap();
    assert_eq!(loaded.manifest.format_version, 2);
    assert_eq!(
        loaded.manifest.schema_version.as_deref(),
        Some(WORLD_MANIFEST_V2_SCHEMA_ID)
    );
    let index = loaded.manifest.world.unwrap();
    assert_eq!(index.city.id, "chicago-loop");
    assert_eq!(index.cell.id, "chicago-loop:e2:n-1");
    assert_eq!(index.cell.grid_index, grid_index);
    assert_eq!(index.cell.local_to_city_enu_m, [970.0, -485.0, 0.0]);
    assert_eq!(index.mobile_cell_policy.maximum_resident_worlds, 2);
    assert_eq!(index.mobile_cell_policy.maximum_prepared_neighbors, 1);
    assert_eq!(index.cell.supported_ranges.geometry_halo_m, 600.0);
    assert_eq!(index.cell.supported_ranges.baked_path_horizon_m, 600.0);
}

#[test]
fn package_v2_rejects_unknown_core_fields_and_v1_v2_hybrids() {
    let bytes = fixture("block.geojson");
    let materials = MaterialTable::default();
    let mesh = compile(
        &GeoJsonProvider::new(&bytes, GeoJsonOptions::default()),
        &materials,
        CompileOptions::default(),
    )
    .unwrap();
    let v2 = TestDirectory::new("package-v2-unknown-core");
    let world = WorldPackageV2Metadata::mobile_cell(
        "test-city",
        GeodeticOrigin::wgs84(0.0, 0.0, 0.0),
        CellGridIndex { east: 0, north: 0 },
        bounds_for(&mesh),
    );
    write_package_v2(&v2.0, &mesh, &materials, &[], "test-tool-2", &world, &[]).unwrap();
    let manifest_path = v2.0.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["future_core_field"] = serde_json::Value::Bool(true);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        read_package(&v2.0),
        Err(WorldError::InvalidPackage(message)) if message.contains("strict v2 schema")
    ));

    let v1 = TestDirectory::new("package-v1-v2-hybrid");
    write_package(&v1.0, &mesh, &materials, &[], "test-tool-1").unwrap();
    let manifest_path = v1.0.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = serde_json::Value::String(WORLD_MANIFEST_V2_SCHEMA_ID.to_owned());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        read_package(&v1.0),
        Err(WorldError::InvalidPackage(message)) if message.contains("partial v2 envelope")
    ));
}

#[test]
fn package_v2_hashes_and_negotiates_opaque_typed_sidecars() {
    const CAPABILITY: &str = STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY;
    const SIDECAR: &[u8] = b"opaque sidecar test bytes";
    let bytes = fixture("block.geojson");
    let materials = MaterialTable::default();
    let mesh = compile(
        &GeoJsonProvider::new(&bytes, GeoJsonOptions::default()),
        &materials,
        CompileOptions::default(),
    )
    .unwrap();
    let world = WorldPackageV2Metadata::mobile_cell(
        "test-city",
        GeodeticOrigin::wgs84(0.0, 0.0, 0.0),
        CellGridIndex { east: 0, north: 0 },
        bounds_for(&mesh),
    );
    let optional = TestDirectory::new("package-v2-optional-sidecar");
    fs::create_dir(optional.0.join("capabilities")).unwrap();
    let stored_sidecar = zstd::stream::encode_all(SIDECAR, 3).unwrap();
    fs::write(
        optional.0.join("capabilities/probes.bin.zst"),
        &stored_sidecar,
    )
    .unwrap();
    let extension = CapabilityExtension::zstd(
        CAPABILITY,
        ExtensionRequirement::Optional,
        "capabilities/probes.bin.zst",
        SIDECAR,
        &stored_sidecar,
    );
    let mut oversized = CapabilityExtension::uncompressed(
        CAPABILITY,
        ExtensionRequirement::Optional,
        "capabilities/oversized.bin",
        SIDECAR,
    );
    oversized.raw_size_bytes = 64 * 1024 * 1024 + 1;
    oversized.stored_size_bytes = oversized.raw_size_bytes;
    assert!(matches!(
        oversized.validate(),
        Err(WorldError::InvalidPackage(message)) if message.contains("mobile hard limit")
    ));
    write_package_v2(
        &optional.0,
        &mesh,
        &materials,
        &[],
        "test-tool-2",
        &world,
        std::slice::from_ref(&extension),
    )
    .unwrap();
    assert_eq!(
        read_package(&optional.0)
            .unwrap()
            .unsupported_optional_capabilities,
        [CAPABILITY]
    );
    assert!(
        read_package_with_capabilities(&optional.0, &[CAPABILITY])
            .unwrap()
            .unsupported_optional_capabilities
            .is_empty()
    );
    fs::write(optional.0.join("capabilities/probes.bin.zst"), b"tampered").unwrap();
    assert!(matches!(
        read_package_with_capabilities(&optional.0, &[CAPABILITY]),
        Err(WorldError::InvalidPackage(message)) if message.contains("stored size") || message.contains("content hash")
    ));

    let required = TestDirectory::new("package-v2-required-sidecar");
    fs::create_dir(required.0.join("capabilities")).unwrap();
    fs::write(required.0.join("capabilities/probes.bin"), SIDECAR).unwrap();
    let required_extension = CapabilityExtension::uncompressed(
        CAPABILITY,
        ExtensionRequirement::Required,
        "capabilities/probes.bin",
        SIDECAR,
    );
    write_package_v2(
        &required.0,
        &mesh,
        &materials,
        &[],
        "test-tool-2",
        &world,
        &[required_extension],
    )
    .unwrap();
    assert!(matches!(
        read_package(&required.0),
        Err(WorldError::InvalidPackage(message)) if message.contains("requires unsupported")
    ));
    read_package_with_capabilities(&required.0, &[CAPABILITY]).unwrap();
}

#[test]
fn loader_rejects_tampered_mesh_and_materials() {
    let bytes = fixture("block.geojson");
    let materials = MaterialTable::default();
    let mesh = compile(
        &GeoJsonProvider::new(&bytes, GeoJsonOptions::default()),
        &materials,
        CompileOptions::default(),
    )
    .unwrap();
    let mesh_dir = TestDirectory::new("tamper-mesh");
    write_package(&mesh_dir.0, &mesh, &materials, &[], "test-tool-1").unwrap();
    let mut mesh_bytes = fs::read(mesh_dir.0.join("mesh.bin")).unwrap();
    *mesh_bytes.last_mut().unwrap() ^= 1;
    fs::write(mesh_dir.0.join("mesh.bin"), mesh_bytes).unwrap();
    assert!(matches!(
        read_package(&mesh_dir.0),
        Err(WorldError::HashMismatch { item: "mesh" })
    ));

    let materials_dir = TestDirectory::new("tamper-materials");
    write_package(&materials_dir.0, &mesh, &materials, &[], "test-tool-1").unwrap();
    fs::write(materials_dir.0.join("materials.json"), b"{}").unwrap();
    assert!(matches!(
        read_package(&materials_dir.0),
        Err(WorldError::HashMismatch { item: "materials" })
    ));
}
