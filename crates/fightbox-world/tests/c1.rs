use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fightbox_api::EnuVector3;
use fightbox_world::{
    CompileOptions, GeoJsonOptions, GeoJsonProvider, Material, MaterialTable, ObjProvider,
    Provenance, ProviderGeometry, TriangleProvider, WorldError, compile, export_obj,
    mesh_content_hash, read_package, write_package,
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
    assert_eq!(loaded.manifest.mesh_content_sha256, expected_hash);
    assert_eq!(mesh_content_hash(&loaded.mesh).unwrap(), expected_hash);
    assert_eq!(loaded.mesh, mesh);
    assert_eq!(loaded.materials, materials);
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
