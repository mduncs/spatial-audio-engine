//! Offline mesh extraction for `fightbox.echo-authority.v1`.
//!
//! Runtime code never calls this module. It converts a validated acoustic mesh
//! and explicit static anchors/listener samples into the deterministic table
//! consumed by [`crate::EchoAuthorityTable`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use fightbox_api::EnuVector3;
use serde::{Deserialize, Serialize};

use crate::{
    AcousticMesh, EchoAuthorityBindings, EchoAuthorityDeclaration, EchoAuthorityError,
    EchoAuthorityTable, EchoPathKind, EchoPathRecord, EchoPathVertex, ExplicitNoPlanReason,
    ListenerNode, MaterialTable, PlanTile, PlanTileContent, Sha256Digest, StableGeometryKey,
    StablePathKey, StableSpatialKey, StaticSourceAnchor, echo_authority::prune_bake_candidates,
};

const BAKER_REVISION: u32 = 1;
const SOUND_CONTRACT_REVISION: u32 = 1;
const SOUND_SPEED_M_S: f64 = 343.0;
const SPECULAR_MIN_EXCESS_M: f64 = 0.3 * SOUND_SPEED_M_S;
const SPECULAR_MAX_EXCESS_M: f64 = 1.2 * SOUND_SPEED_M_S;
const MAX_TOTAL_PATH_M: f64 = 2_048.0;
const CORNER_PRESSURE: [f32; 3] = [0.354_813_4, 0.177_827_94, 0.063_095_73];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EchoExtractorConfig {
    pub weld_tolerance_m: f64,
    pub facade_vertical_tolerance_degrees: f64,
    pub coplanar_normal_tolerance_degrees: f64,
    pub coplanar_distance_tolerance_m: f64,
    pub ray_epsilon_m: f64,
    pub boundary_tolerance_m: f64,
    pub minimum_reflector_area_m2: f64,
    pub minimum_vertical_extent_m: f64,
    pub minimum_in_plane_extent_m: f64,
}

impl Default for EchoExtractorConfig {
    fn default() -> Self {
        Self {
            weld_tolerance_m: 0.001,
            facade_vertical_tolerance_degrees: 2.0,
            coplanar_normal_tolerance_degrees: 0.5,
            coplanar_distance_tolerance_m: 0.010,
            ray_epsilon_m: 0.010,
            boundary_tolerance_m: 0.020,
            minimum_reflector_area_m2: 8.0,
            minimum_vertical_extent_m: 2.0,
            minimum_in_plane_extent_m: 1.0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EchoExtractionBindings {
    pub package_manifest_hash: Sha256Digest,
    pub mesh_hash: Sha256Digest,
    pub material_hash: Sha256Digest,
    pub coordinate_frame: StableSpatialKey,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoListenerSample {
    pub key: StableSpatialKey,
    pub position_city_enu_m: [f32; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoListenerCoverage {
    pub cell: StableSpatialKey,
    pub samples: Vec<EchoListenerSample>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoExtractionRequest {
    pub bindings: EchoExtractionBindings,
    pub anchors: Vec<StaticSourceAnchor>,
    pub listener_coverage: Vec<EchoListenerCoverage>,
    /// Must be supplied by the importer. The current GeoJSON provider does not
    /// preserve ring topology, so any nonzero value is rejected explicitly.
    pub discarded_polygon_hole_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FacadePatch {
    pub key: StableGeometryKey,
    pub material_id: u32,
    pub triangle_indices: Vec<u32>,
    pub boundary_vertices_city_enu_m: Vec<[f32; 3]>,
    pub normal_city_enu: [f32; 3],
    pub area_m2: f32,
    pub vertical_extent_m: f32,
    pub shortest_in_plane_extent_m: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffractionEdge {
    pub key: StableGeometryKey,
    pub endpoints_city_enu_m: [[f32; 3]; 2],
    pub adjacent_patches: Vec<StableGeometryKey>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EchoExtractionStats {
    pub input_vertices: usize,
    pub welded_vertices: usize,
    pub input_triangles: usize,
    pub eligible_facade_triangles: usize,
    pub emitted_facade_patches: usize,
    pub rejected_small_patches: usize,
    pub emitted_diffraction_edges: usize,
    pub rejected_nonmanifold_edges: usize,
    pub listener_nodes: usize,
    pub plan_tiles: usize,
    pub explicit_no_plan_tiles: usize,
    pub emitted_candidates: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EchoAuthorityArtifactManifest {
    pub schema_id: String,
    pub binary_file: String,
    pub binary_magic: String,
    pub table_sha256: String,
    pub table_size_bytes: u64,
    pub package_manifest_sha256: String,
    pub mesh_sha256: String,
    pub material_sha256: String,
    pub fixture_request_sha256: String,
    pub anchor_set_sha256: String,
    pub probe_layout_sha256: String,
    pub coordinate_frame_key: String,
    pub baker_revision: u32,
    pub sound_contract_revision: u32,
    pub speed_of_sound_m_s: u32,
    pub weld_tolerance_mm: u32,
    pub facade_vertical_tolerance_millidegrees: u32,
    pub coplanar_normal_tolerance_millidegrees: u32,
    pub coplanar_distance_tolerance_mm: u32,
    pub ray_epsilon_mm: u32,
    pub boundary_tolerance_mm: u32,
    pub minimum_reflector_area_square_centimeters: u32,
    pub minimum_vertical_extent_mm: u32,
    pub minimum_in_plane_extent_mm: u32,
    pub maximum_total_path_m: u32,
    pub specular_excess_delay_window_ms: [u32; 2],
    pub discarded_polygon_hole_count: usize,
    pub stats: EchoExtractionStats,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoExtractionArtifact {
    pub table: EchoAuthorityTable,
    pub manifest: EchoAuthorityArtifactManifest,
    pub patches: Vec<FacadePatch>,
    pub diffraction_edges: Vec<DiffractionEdge>,
}

impl EchoExtractionArtifact {
    pub fn table_bytes(&self) -> Vec<u8> {
        self.table.encode()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EchoExtractionError {
    InvalidConfig(String),
    InvalidMesh(String),
    InvalidRequest(String),
    UnsupportedPolygonHoles { discarded_count: usize },
    UnsupportedListenerTopology(String),
    Authority(EchoAuthorityError),
}

impl fmt::Display for EchoExtractionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid echo extractor config: {message}"),
            Self::InvalidMesh(message) => write!(f, "invalid echo extractor mesh: {message}"),
            Self::InvalidRequest(message) => {
                write!(f, "invalid echo extraction request: {message}")
            }
            Self::UnsupportedPolygonHoles { discarded_count } => write!(
                f,
                "echo extraction refuses {discarded_count} discarded GeoJSON polygon hole(s); preserve and extrude interior rings before baking authority"
            ),
            Self::UnsupportedListenerTopology(message) => {
                write!(f, "unsupported listener topology: {message}")
            }
            Self::Authority(error) => write!(f, "cannot build echo authority: {error}"),
        }
    }
}

impl std::error::Error for EchoExtractionError {}

impl From<EchoAuthorityError> for EchoExtractionError {
    fn from(value: EchoAuthorityError) -> Self {
        Self::Authority(value)
    }
}

pub struct EchoExtractor {
    config: EchoExtractorConfig,
}

impl EchoExtractor {
    pub fn new(config: EchoExtractorConfig) -> Result<Self, EchoExtractionError> {
        validate_config(config)?;
        Ok(Self { config })
    }

    /// Welds `mesh` and returns only its finite reflector patches and vertical
    /// diffraction edges, for a host that plans echo paths at run time.
    pub fn facade_geometry(
        &self,
        mesh: &AcousticMesh,
        materials: &MaterialTable,
    ) -> Result<(Vec<FacadePatch>, Vec<DiffractionEdge>), EchoExtractionError> {
        materials
            .validate()
            .map_err(|error| EchoExtractionError::InvalidMesh(error.to_string()))?;
        mesh.validate(materials.iter().len(), usize::MAX)
            .map_err(|error| EchoExtractionError::InvalidMesh(error.to_string()))?;
        let welded = weld_mesh(mesh, self.config.weld_tolerance_m)?;
        let patches = extract_patches(&welded, self.config, &mut EchoExtractionStats::default());
        let (edges, _) = extract_diffraction_edges(&welded, &patches, self.config);
        Ok((
            patches.iter().map(|patch| patch.public(&welded)).collect(),
            edges
                .iter()
                .map(|edge| edge.public(&welded, &patches))
                .collect(),
        ))
    }

    pub fn extract(
        &self,
        mesh: &AcousticMesh,
        materials: &MaterialTable,
        mut request: EchoExtractionRequest,
    ) -> Result<EchoExtractionArtifact, EchoExtractionError> {
        if request.discarded_polygon_hole_count != 0 {
            return Err(EchoExtractionError::UnsupportedPolygonHoles {
                discarded_count: request.discarded_polygon_hole_count,
            });
        }
        materials
            .validate()
            .map_err(|error| EchoExtractionError::InvalidMesh(error.to_string()))?;
        mesh.validate(materials.iter().len(), usize::MAX)
            .map_err(|error| EchoExtractionError::InvalidMesh(error.to_string()))?;
        validate_request(&request)?;
        request.anchors.sort_by_key(|anchor| anchor.key);
        request
            .listener_coverage
            .sort_by_key(|coverage| coverage.cell);
        for coverage in &mut request.listener_coverage {
            coverage.samples.sort_by_key(|sample| sample.key);
        }

        let mut stats = EchoExtractionStats {
            input_vertices: mesh.vertices_enu_m.len(),
            input_triangles: mesh.triangles.len(),
            ..EchoExtractionStats::default()
        };
        let welded = weld_mesh(mesh, self.config.weld_tolerance_m)?;
        stats.welded_vertices = welded.vertices.len();
        let internal_patches = extract_patches(&welded, self.config, &mut stats);
        let (internal_edges, rejected_nonmanifold_edges) =
            extract_diffraction_edges(&welded, &internal_patches, self.config);
        stats.rejected_nonmanifold_edges = rejected_nonmanifold_edges;
        stats.emitted_facade_patches = internal_patches.len();
        stats.emitted_diffraction_edges = internal_edges.len();

        let anchor_set_hash = hash_anchors(&request.anchors);
        let probe_layout_hash = hash_listener_layout(&request.listener_coverage);
        let fixture_request_hash = hash_request(&request, self.config);
        let bindings = EchoAuthorityBindings {
            package_manifest_hash: request.bindings.package_manifest_hash,
            mesh_hash: request.bindings.mesh_hash,
            material_hash: request.bindings.material_hash,
            fixture_request_hash,
            anchor_set_hash,
            probe_layout_hash,
            coordinate_frame: request.bindings.coordinate_frame,
            baker_revision: BAKER_REVISION,
            sound_contract_revision: SOUND_CONTRACT_REVISION,
        };

        let listener_nodes = request
            .listener_coverage
            .iter()
            .flat_map(|coverage| {
                coverage.samples.iter().map(|sample| ListenerNode {
                    key: sample.key,
                    listener_cell: coverage.cell,
                    position_city_enu_m: sample.position_city_enu_m,
                })
            })
            .collect::<Vec<_>>();
        stats.listener_nodes = listener_nodes.len();
        let mut tiles = Vec::new();
        for coverage in &request.listener_coverage {
            let triangles = triangulate_listener_samples(&coverage.samples, self.config)?;
            for anchor in &request.anchors {
                for triangle in &triangles {
                    let tile = build_plan_tile(
                        &welded,
                        materials,
                        &internal_patches,
                        &internal_edges,
                        anchor,
                        coverage,
                        *triangle,
                        self.config,
                    )?;
                    if matches!(tile.content, PlanTileContent::NoPlan(_)) {
                        stats.explicit_no_plan_tiles += 1;
                    }
                    if let PlanTileContent::Paths(paths) = &tile.content {
                        stats.emitted_candidates += paths.len();
                    }
                    tiles.push(tile);
                }
            }
        }
        stats.plan_tiles = tiles.len();
        let table = EchoAuthorityTable::new(bindings, request.anchors, listener_nodes, tiles)?;
        let declaration = EchoAuthorityDeclaration::for_table(&table);
        let manifest = EchoAuthorityArtifactManifest {
            schema_id: crate::ECHO_AUTHORITY_SCHEMA_ID.to_owned(),
            binary_file: "echo-authority.bin".to_owned(),
            binary_magic: "FBXECHO\\0".to_owned(),
            table_sha256: declaration.table_sha256.to_hex(),
            table_size_bytes: declaration.table_bytes,
            package_manifest_sha256: table.bindings().package_manifest_hash.to_hex(),
            mesh_sha256: table.bindings().mesh_hash.to_hex(),
            material_sha256: table.bindings().material_hash.to_hex(),
            fixture_request_sha256: table.bindings().fixture_request_hash.to_hex(),
            anchor_set_sha256: table.bindings().anchor_set_hash.to_hex(),
            probe_layout_sha256: table.bindings().probe_layout_hash.to_hex(),
            coordinate_frame_key: table.bindings().coordinate_frame.to_hex(),
            baker_revision: BAKER_REVISION,
            sound_contract_revision: SOUND_CONTRACT_REVISION,
            speed_of_sound_m_s: SOUND_SPEED_M_S as u32,
            weld_tolerance_mm: (self.config.weld_tolerance_m * 1_000.0).round() as u32,
            facade_vertical_tolerance_millidegrees: (self.config.facade_vertical_tolerance_degrees
                * 1_000.0)
                .round() as u32,
            coplanar_normal_tolerance_millidegrees: (self.config.coplanar_normal_tolerance_degrees
                * 1_000.0)
                .round() as u32,
            coplanar_distance_tolerance_mm: (self.config.coplanar_distance_tolerance_m * 1_000.0)
                .round() as u32,
            ray_epsilon_mm: (self.config.ray_epsilon_m * 1_000.0).round() as u32,
            boundary_tolerance_mm: (self.config.boundary_tolerance_m * 1_000.0).round() as u32,
            minimum_reflector_area_square_centimeters: (self.config.minimum_reflector_area_m2
                * 10_000.0)
                .round() as u32,
            minimum_vertical_extent_mm: (self.config.minimum_vertical_extent_m * 1_000.0).round()
                as u32,
            minimum_in_plane_extent_mm: (self.config.minimum_in_plane_extent_m * 1_000.0).round()
                as u32,
            maximum_total_path_m: MAX_TOTAL_PATH_M as u32,
            specular_excess_delay_window_ms: [300, 1_200],
            discarded_polygon_hole_count: 0,
            stats: stats.clone(),
        };
        Ok(EchoExtractionArtifact {
            table,
            manifest,
            patches: internal_patches
                .iter()
                .map(|patch| patch.public(&welded))
                .collect(),
            diffraction_edges: internal_edges
                .iter()
                .map(|edge| edge.public(&welded, &internal_patches))
                .collect(),
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Vec3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vec3 {
    fn from_enu(value: EnuVector3) -> Self {
        Self {
            x: f64::from(value.east_m),
            y: f64::from(value.north_m),
            z: f64::from(value.up_m),
        }
    }

    fn from_f32(value: [f32; 3]) -> Self {
        Self {
            x: f64::from(value[0]),
            y: f64::from(value[1]),
            z: f64::from(value[2]),
        }
    }

    fn f32(self) -> [f32; 3] {
        [self.x as f32, self.y as f32, self.z as f32]
    }

    fn add(self, other: Self) -> Self {
        Self {
            x: self.x + other.x,
            y: self.y + other.y,
            z: self.z + other.z,
        }
    }

    fn sub(self, other: Self) -> Self {
        Self {
            x: self.x - other.x,
            y: self.y - other.y,
            z: self.z - other.z,
        }
    }

    fn scale(self, scalar: f64) -> Self {
        Self {
            x: self.x * scalar,
            y: self.y * scalar,
            z: self.z * scalar,
        }
    }

    fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    fn cross(self, other: Self) -> Self {
        Self {
            x: self.y * other.z - self.z * other.y,
            y: self.z * other.x - self.x * other.z,
            z: self.x * other.y - self.y * other.x,
        }
    }

    fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    fn normalize(self) -> Self {
        self.scale(1.0 / self.length())
    }
}

#[derive(Clone, Debug)]
struct WeldedTriangle {
    original_index: usize,
    vertices: [usize; 3],
    material_id: u32,
    normal: Vec3,
    area_m2: f64,
}

#[derive(Clone, Debug)]
struct WeldedMesh {
    vertices: Vec<Vec3>,
    triangles: Vec<WeldedTriangle>,
}

#[derive(Clone, Debug)]
struct FacadePatchInternal {
    key: StableGeometryKey,
    material_id: u32,
    triangles: Vec<usize>,
    boundary_edges: Vec<[usize; 2]>,
    normal: Vec3,
    plane_offset: f64,
    area_m2: f64,
    vertical_extent_m: f64,
    horizontal_extent_m: f64,
}

impl FacadePatchInternal {
    fn public(&self, mesh: &WeldedMesh) -> FacadePatch {
        let boundary_vertices = self
            .boundary_edges
            .iter()
            .flat_map(|edge| *edge)
            .collect::<BTreeSet<_>>();
        FacadePatch {
            key: self.key,
            material_id: self.material_id,
            triangle_indices: self
                .triangles
                .iter()
                .map(|index| mesh.triangles[*index].original_index as u32)
                .collect(),
            boundary_vertices_city_enu_m: boundary_vertices
                .into_iter()
                .map(|vertex| mesh.vertices[vertex].f32())
                .collect(),
            normal_city_enu: self.normal.f32(),
            area_m2: self.area_m2 as f32,
            vertical_extent_m: self.vertical_extent_m as f32,
            shortest_in_plane_extent_m: self.horizontal_extent_m.min(self.vertical_extent_m) as f32,
        }
    }
}

#[derive(Clone, Debug)]
struct DiffractionEdgeInternal {
    key: StableGeometryKey,
    vertices: [usize; 2],
    adjacent_patches: Vec<usize>,
}

impl DiffractionEdgeInternal {
    fn public(&self, mesh: &WeldedMesh, patches: &[FacadePatchInternal]) -> DiffractionEdge {
        DiffractionEdge {
            key: self.key,
            endpoints_city_enu_m: self.vertices.map(|vertex| mesh.vertices[vertex].f32()),
            adjacent_patches: self
                .adjacent_patches
                .iter()
                .map(|index| patches[*index].key)
                .collect(),
        }
    }
}

fn validate_config(config: EchoExtractorConfig) -> Result<(), EchoExtractionError> {
    let finite_positive = [
        config.weld_tolerance_m,
        config.facade_vertical_tolerance_degrees,
        config.coplanar_normal_tolerance_degrees,
        config.coplanar_distance_tolerance_m,
        config.ray_epsilon_m,
        config.boundary_tolerance_m,
        config.minimum_reflector_area_m2,
        config.minimum_vertical_extent_m,
        config.minimum_in_plane_extent_m,
    ]
    .into_iter()
    .all(|value| value.is_finite() && value > 0.0);
    if !finite_positive
        || config.facade_vertical_tolerance_degrees >= 90.0
        || config.coplanar_normal_tolerance_degrees >= 90.0
    {
        return Err(EchoExtractionError::InvalidConfig(
            "tolerances and dimensions must be finite, positive, and angular tolerances below 90 degrees".to_owned(),
        ));
    }
    Ok(())
}

fn validate_request(request: &EchoExtractionRequest) -> Result<(), EchoExtractionError> {
    if request.anchors.is_empty() {
        return Err(EchoExtractionError::InvalidRequest(
            "at least one static anchor is required".to_owned(),
        ));
    }
    if request.listener_coverage.is_empty() {
        return Err(EchoExtractionError::InvalidRequest(
            "at least one listener cell is required".to_owned(),
        ));
    }
    if request.bindings.package_manifest_hash.is_zero()
        || request.bindings.mesh_hash.is_zero()
        || request.bindings.material_hash.is_zero()
        || request.bindings.coordinate_frame.is_zero()
    {
        return Err(EchoExtractionError::InvalidRequest(
            "hash and coordinate-frame bindings must be nonzero".to_owned(),
        ));
    }
    let mut anchors = BTreeSet::new();
    for anchor in &request.anchors {
        if !anchors.insert(anchor.key) || !finite_f32(anchor.position_city_enu_m) {
            return Err(EchoExtractionError::InvalidRequest(
                "anchor keys must be unique and positions finite".to_owned(),
            ));
        }
    }
    let mut cells = BTreeSet::new();
    let mut samples = BTreeSet::new();
    for coverage in &request.listener_coverage {
        if coverage.cell.is_zero() || !cells.insert(coverage.cell) || coverage.samples.len() < 3 {
            return Err(EchoExtractionError::InvalidRequest(
                "listener cells must be unique, nonzero, and contain at least three samples"
                    .to_owned(),
            ));
        }
        for sample in &coverage.samples {
            if sample.key.is_zero()
                || !samples.insert(sample.key)
                || !finite_f32(sample.position_city_enu_m)
            {
                return Err(EchoExtractionError::InvalidRequest(
                    "listener sample keys must be globally unique and positions finite".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn weld_mesh(mesh: &AcousticMesh, tolerance: f64) -> Result<WeldedMesh, EchoExtractionError> {
    let mut ordered = mesh
        .vertices_enu_m
        .iter()
        .copied()
        .enumerate()
        .map(|(index, point)| (index, Vec3::from_enu(point)))
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.1
            .x
            .total_cmp(&right.1.x)
            .then_with(|| left.1.y.total_cmp(&right.1.y))
            .then_with(|| left.1.z.total_cmp(&right.1.z))
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut vertices = Vec::<Vec3>::new();
    let mut buckets = BTreeMap::<[i64; 3], Vec<usize>>::new();
    let mut remap = vec![0; mesh.vertices_enu_m.len()];
    for (original, point) in ordered {
        let cell = quantized_cell(point, tolerance);
        let mut match_index = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let neighbor = [cell[0] + dx, cell[1] + dy, cell[2] + dz];
                    for candidate in buckets.get(&neighbor).into_iter().flatten() {
                        if point.sub(vertices[*candidate]).length() <= tolerance {
                            match_index = Some(
                                match_index.map_or(*candidate, |old: usize| old.min(*candidate)),
                            );
                        }
                    }
                }
            }
        }
        let welded = match_index.unwrap_or_else(|| {
            let index = vertices.len();
            vertices.push(point);
            buckets.entry(cell).or_default().push(index);
            index
        });
        remap[original] = welded;
    }
    let mut triangles = Vec::with_capacity(mesh.triangles.len());
    for (index, (triangle, material_id)) in
        mesh.triangles.iter().zip(&mesh.material_ids).enumerate()
    {
        let vertices_index = triangle.map(|source| remap[source as usize]);
        if vertices_index[0] == vertices_index[1]
            || vertices_index[1] == vertices_index[2]
            || vertices_index[2] == vertices_index[0]
        {
            return Err(EchoExtractionError::InvalidMesh(format!(
                "triangle {index} collapses under the 1 mm weld tolerance"
            )));
        }
        let a = vertices[vertices_index[0]];
        let b = vertices[vertices_index[1]];
        let c = vertices[vertices_index[2]];
        let cross = b.sub(a).cross(c.sub(a));
        let area_m2 = cross.length() * 0.5;
        triangles.push(WeldedTriangle {
            original_index: index,
            vertices: vertices_index,
            material_id: *material_id,
            normal: cross.normalize(),
            area_m2,
        });
    }
    Ok(WeldedMesh {
        vertices,
        triangles,
    })
}

fn extract_patches(
    mesh: &WeldedMesh,
    config: EchoExtractorConfig,
    stats: &mut EchoExtractionStats,
) -> Vec<FacadePatchInternal> {
    let vertical_sine = config.facade_vertical_tolerance_degrees.to_radians().sin();
    let normal_cosine = config.coplanar_normal_tolerance_degrees.to_radians().cos();
    let eligible = mesh
        .triangles
        .iter()
        .enumerate()
        .filter(|(_, triangle)| triangle.normal.z.abs() <= vertical_sine)
        .map(|(index, _)| index)
        .collect::<BTreeSet<_>>();
    stats.eligible_facade_triangles = eligible.len();
    let mut edge_triangles = BTreeMap::<[usize; 2], Vec<usize>>::new();
    for index in &eligible {
        for edge in triangle_edges(mesh.triangles[*index].vertices) {
            edge_triangles.entry(edge).or_default().push(*index);
        }
    }
    let mut unvisited = eligible;
    let mut patches = Vec::new();
    while let Some(seed) = unvisited.pop_first() {
        let seed_triangle = &mesh.triangles[seed];
        let seed_point = mesh.vertices[seed_triangle.vertices[0]];
        let seed_plane = seed_triangle.normal.dot(seed_point);
        let mut queue = VecDeque::from([seed]);
        let mut component = Vec::new();
        while let Some(index) = queue.pop_front() {
            component.push(index);
            for edge in triangle_edges(mesh.triangles[index].vertices) {
                for neighbor in edge_triangles.get(&edge).into_iter().flatten() {
                    if !unvisited.contains(neighbor) {
                        continue;
                    }
                    let triangle = &mesh.triangles[*neighbor];
                    let coplanar = triangle.material_id == seed_triangle.material_id
                        && triangle.normal.dot(seed_triangle.normal) >= normal_cosine
                        && triangle.vertices.iter().all(|vertex| {
                            (seed_triangle.normal.dot(mesh.vertices[*vertex]) - seed_plane).abs()
                                <= config.coplanar_distance_tolerance_m
                        });
                    if coplanar {
                        unvisited.remove(neighbor);
                        queue.push_back(*neighbor);
                    }
                }
            }
        }
        component.sort_unstable();
        let area_m2 = component
            .iter()
            .map(|index| mesh.triangles[*index].area_m2)
            .sum::<f64>();
        let vertex_ids = component
            .iter()
            .flat_map(|index| mesh.triangles[*index].vertices)
            .collect::<BTreeSet<_>>();
        let normal_sum = component.iter().fold(Vec3::default(), |sum, index| {
            sum.add(
                mesh.triangles[*index]
                    .normal
                    .scale(mesh.triangles[*index].area_m2),
            )
        });
        let normal = normal_sum.normalize();
        let plane_offset = vertex_ids
            .iter()
            .map(|vertex| normal.dot(mesh.vertices[*vertex]))
            .sum::<f64>()
            / vertex_ids.len() as f64;
        let horizontal_axis = Vec3 {
            x: -normal.y,
            y: normal.x,
            z: 0.0,
        }
        .normalize();
        let (min_z, max_z, min_horizontal, max_horizontal) = vertex_ids.iter().fold(
            (
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
            ),
            |(min_z, max_z, min_h, max_h), vertex| {
                let point = mesh.vertices[*vertex];
                let horizontal = point.dot(horizontal_axis);
                (
                    min_z.min(point.z),
                    max_z.max(point.z),
                    min_h.min(horizontal),
                    max_h.max(horizontal),
                )
            },
        );
        let vertical_extent_m = max_z - min_z;
        let horizontal_extent_m = max_horizontal - min_horizontal;
        if area_m2 < config.minimum_reflector_area_m2
            || vertical_extent_m < config.minimum_vertical_extent_m
            || horizontal_extent_m < config.minimum_in_plane_extent_m
        {
            stats.rejected_small_patches += 1;
            continue;
        }
        let mut edge_counts = BTreeMap::<[usize; 2], usize>::new();
        for index in &component {
            for edge in triangle_edges(mesh.triangles[*index].vertices) {
                *edge_counts.entry(edge).or_default() += 1;
            }
        }
        let boundary_edges = edge_counts
            .into_iter()
            .filter_map(|(edge, count)| (count == 1).then_some(edge))
            .collect::<Vec<_>>();
        let key = patch_key(mesh, seed_triangle.material_id, &component);
        patches.push(FacadePatchInternal {
            key,
            material_id: seed_triangle.material_id,
            triangles: component,
            boundary_edges,
            normal,
            plane_offset,
            area_m2,
            vertical_extent_m,
            horizontal_extent_m,
        });
    }
    patches.sort_by_key(|patch| patch.key);
    patches
}

fn extract_diffraction_edges(
    mesh: &WeldedMesh,
    patches: &[FacadePatchInternal],
    config: EchoExtractorConfig,
) -> (Vec<DiffractionEdgeInternal>, usize) {
    let vertical_sine = config.facade_vertical_tolerance_degrees.to_radians().sin();
    let mut occurrences = BTreeMap::<[usize; 2], Vec<usize>>::new();
    for (patch_index, patch) in patches.iter().enumerate() {
        for edge in &patch.boundary_edges {
            let direction = mesh.vertices[edge[1]].sub(mesh.vertices[edge[0]]);
            let length = direction.length();
            let horizontal = (direction.x * direction.x + direction.y * direction.y).sqrt();
            if length >= config.minimum_vertical_extent_m && horizontal / length <= vertical_sine {
                occurrences.entry(*edge).or_default().push(patch_index);
            }
        }
    }
    let mut rejected_nonmanifold = 0;
    let mut result = Vec::new();
    for (vertices, mut adjacent_patches) in occurrences {
        adjacent_patches.sort_by_key(|index| patches[*index].key);
        adjacent_patches.dedup();
        if adjacent_patches.len() > 2 {
            rejected_nonmanifold += 1;
            continue;
        }
        if adjacent_patches.len() == 2
            && patches[adjacent_patches[0]]
                .normal
                .dot(patches[adjacent_patches[1]].normal)
                .abs()
                >= config.coplanar_normal_tolerance_degrees.to_radians().cos()
        {
            // A material or clustering seam in one plane is not a corner.
            continue;
        }
        let mut bytes = geometry_edge_bytes(mesh.vertices[vertices[0]], mesh.vertices[vertices[1]]);
        for patch in &adjacent_patches {
            bytes.extend_from_slice(&patches[*patch].key.0);
        }
        result.push(DiffractionEdgeInternal {
            key: StableGeometryKey::derive("echo-diffraction-edge-v1", &bytes),
            vertices,
            adjacent_patches,
        });
    }
    result.sort_by_key(|edge| edge.key);
    (result, rejected_nonmanifold)
}

fn build_plan_tile(
    mesh: &WeldedMesh,
    materials: &MaterialTable,
    patches: &[FacadePatchInternal],
    edges: &[DiffractionEdgeInternal],
    anchor: &StaticSourceAnchor,
    coverage: &EchoListenerCoverage,
    triangle: [usize; 3],
    config: EchoExtractorConfig,
) -> Result<PlanTile, EchoExtractionError> {
    let listeners =
        triangle.map(|index| Vec3::from_f32(coverage.samples[index].position_city_enu_m));
    let source = Vec3::from_f32(anchor.position_city_enu_m);
    let direct_occluded = listeners.map(|listener| {
        segment_occluded(
            mesh,
            source,
            listener,
            &BTreeSet::new(),
            config.ray_epsilon_m,
        )
    });
    let keys = triangle.map(|index| coverage.samples[index].key);
    let tile_key = plan_tile_key(anchor.key, coverage.cell, keys);
    if direct_occluded
        .iter()
        .any(|value| *value != direct_occluded[0])
    {
        return Ok(PlanTile {
            key: tile_key,
            source_anchor: anchor.key,
            listener_cell: coverage.cell,
            listener_vertices: keys,
            direct_occluded: true,
            content: PlanTileContent::NoPlan(ExplicitNoPlanReason::VisibilityDiscontinuity),
        });
    }
    let mut paths = Vec::new();
    let mut discontinuity = false;
    for patch in patches {
        let samples = listeners
            .map(|listener| specular_sample(mesh, materials, patch, source, listener, config));
        match collect_stable_samples(samples) {
            StableSamples::All(vertices) => paths.push(EchoPathRecord {
                path_key: path_key(anchor.key, patch.key, EchoPathKind::Specular),
                geometry_key: patch.key,
                material_key: material_key(materials, patch.material_id)?,
                kind: EchoPathKind::Specular,
                vertices,
            }),
            StableSamples::Mixed => discontinuity = true,
            StableSamples::None => {}
        }
    }
    if direct_occluded[0] {
        for edge in edges {
            let samples = listeners
                .map(|listener| diffraction_sample(mesh, patches, edge, source, listener, config));
            match collect_stable_samples(samples) {
                StableSamples::All(vertices) => paths.push(EchoPathRecord {
                    path_key: path_key(anchor.key, edge.key, EchoPathKind::Diffraction),
                    geometry_key: edge.key,
                    material_key: edge_material_key(materials, patches, edge)?,
                    kind: EchoPathKind::Diffraction,
                    vertices,
                }),
                StableSamples::Mixed => discontinuity = true,
                StableSamples::None => {}
            }
        }
    }
    if discontinuity {
        return Ok(PlanTile {
            key: tile_key,
            source_anchor: anchor.key,
            listener_cell: coverage.cell,
            listener_vertices: keys,
            direct_occluded: direct_occluded[0],
            content: PlanTileContent::NoPlan(ExplicitNoPlanReason::VisibilityDiscontinuity),
        });
    }
    prune_bake_candidates(&mut paths);
    Ok(PlanTile {
        key: tile_key,
        source_anchor: anchor.key,
        listener_cell: coverage.cell,
        listener_vertices: keys,
        direct_occluded: direct_occluded[0],
        content: PlanTileContent::Paths(paths),
    })
}

enum StableSamples {
    All([EchoPathVertex; 3]),
    None,
    Mixed,
}

fn collect_stable_samples(samples: [Option<EchoPathVertex>; 3]) -> StableSamples {
    match samples {
        [Some(a), Some(b), Some(c)] => StableSamples::All([a, b, c]),
        [None, None, None] => StableSamples::None,
        _ => StableSamples::Mixed,
    }
}

fn specular_sample(
    mesh: &WeldedMesh,
    materials: &MaterialTable,
    patch: &FacadePatchInternal,
    source: Vec3,
    listener: Vec3,
    config: EchoExtractorConfig,
) -> Option<EchoPathVertex> {
    let mirrored = source.sub(
        patch
            .normal
            .scale(2.0 * (patch.normal.dot(source) - patch.plane_offset)),
    );
    let ray = mirrored.sub(listener);
    let denominator = patch.normal.dot(ray);
    if denominator.abs() < 1.0e-9 {
        return None;
    }
    let t = (patch.plane_offset - patch.normal.dot(listener)) / denominator;
    if !(0.0..1.0).contains(&t) {
        return None;
    }
    let bounce = listener.add(ray.scale(t));
    if !point_on_patch(mesh, patch, bounce, config.boundary_tolerance_m) {
        return None;
    }
    let excluded = patch.triangles.iter().copied().collect::<BTreeSet<_>>();
    if segment_occluded(mesh, source, bounce, &excluded, config.ray_epsilon_m)
        || segment_occluded(mesh, bounce, listener, &excluded, config.ray_epsilon_m)
    {
        return None;
    }
    let total_path_m = source.sub(bounce).length() + listener.sub(bounce).length();
    let direct_path_m = source.sub(listener).length();
    let excess_path_m = total_path_m - direct_path_m;
    if total_path_m > MAX_TOTAL_PATH_M
        || !(SPECULAR_MIN_EXCESS_M..=SPECULAR_MAX_EXCESS_M).contains(&excess_path_m)
    {
        return None;
    }
    let material = materials.iter().nth(patch.material_id as usize)?.1;
    let material_pressure = material
        .absorption
        .map(|absorption| (1.0 - absorption).max(0.0).sqrt());
    let predicted_received_pressure =
        material_pressure.map(|pressure| pressure / total_path_m as f32);
    Some(EchoPathVertex {
        total_path_m: total_path_m as f32,
        excess_path_m: excess_path_m as f32,
        final_interaction_city_enu_m: bounce.f32(),
        arrival_vector_city_enu_m: listener.sub(bounce).f32(),
        material_pressure,
        predicted_received_pressure,
    })
}

fn diffraction_sample(
    mesh: &WeldedMesh,
    patches: &[FacadePatchInternal],
    edge: &DiffractionEdgeInternal,
    source: Vec3,
    listener: Vec3,
    config: EchoExtractorConfig,
) -> Option<EchoPathVertex> {
    let start = mesh.vertices[edge.vertices[0]];
    let end = mesh.vertices[edge.vertices[1]];
    let direction = end.sub(start);
    let cost = |t: f64| {
        let point = start.add(direction.scale(t));
        source.sub(point).length() + listener.sub(point).length()
    };
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..32 {
        let left = low + (high - low) / 3.0;
        let right = high - (high - low) / 3.0;
        if cost(left) <= cost(right) {
            high = right;
        } else {
            low = left;
        }
    }
    let interaction = start.add(direction.scale((low + high) * 0.5));
    let excluded = edge
        .adjacent_patches
        .iter()
        .flat_map(|index| patches[*index].triangles.iter().copied())
        .collect::<BTreeSet<_>>();
    if segment_occluded(mesh, source, interaction, &excluded, config.ray_epsilon_m)
        || segment_occluded(mesh, interaction, listener, &excluded, config.ray_epsilon_m)
    {
        return None;
    }
    let total_path_m = cost((low + high) * 0.5);
    if total_path_m > MAX_TOTAL_PATH_M {
        return None;
    }
    let direct_path_m = source.sub(listener).length();
    Some(EchoPathVertex {
        total_path_m: total_path_m as f32,
        excess_path_m: (total_path_m - direct_path_m).max(0.0) as f32,
        final_interaction_city_enu_m: interaction.f32(),
        arrival_vector_city_enu_m: listener.sub(interaction).f32(),
        material_pressure: CORNER_PRESSURE,
        predicted_received_pressure: CORNER_PRESSURE.map(|pressure| pressure / total_path_m as f32),
    })
}

fn point_on_patch(
    mesh: &WeldedMesh,
    patch: &FacadePatchInternal,
    point: Vec3,
    tolerance: f64,
) -> bool {
    patch.triangles.iter().any(|index| {
        let vertices = mesh.triangles[*index]
            .vertices
            .map(|vertex| mesh.vertices[vertex]);
        point_in_triangle(point, vertices, tolerance)
    })
}

fn point_in_triangle(point: Vec3, vertices: [Vec3; 3], tolerance: f64) -> bool {
    let v0 = vertices[1].sub(vertices[0]);
    let v1 = vertices[2].sub(vertices[0]);
    let v2 = point.sub(vertices[0]);
    let d00 = v0.dot(v0);
    let d01 = v0.dot(v1);
    let d11 = v1.dot(v1);
    let d20 = v2.dot(v0);
    let d21 = v2.dot(v1);
    let denominator = d00 * d11 - d01 * d01;
    if denominator.abs() < 1.0e-12 {
        return false;
    }
    let v = (d11 * d20 - d01 * d21) / denominator;
    let w = (d00 * d21 - d01 * d20) / denominator;
    let u = 1.0 - v - w;
    let scale = v0.length().min(v1.length()).max(0.001);
    let epsilon = tolerance / scale;
    u >= -epsilon && v >= -epsilon && w >= -epsilon
}

fn segment_occluded(
    mesh: &WeldedMesh,
    start: Vec3,
    end: Vec3,
    excluded_triangles: &BTreeSet<usize>,
    epsilon_m: f64,
) -> bool {
    let direction = end.sub(start);
    let length = direction.length();
    if length <= epsilon_m * 2.0 {
        return false;
    }
    let endpoint_fraction = epsilon_m / length;
    mesh.triangles.iter().enumerate().any(|(index, triangle)| {
        !excluded_triangles.contains(&index)
            && segment_triangle_fraction(
                start,
                direction,
                triangle.vertices.map(|vertex| mesh.vertices[vertex]),
            )
            .is_some_and(|fraction| {
                fraction > endpoint_fraction && fraction < 1.0 - endpoint_fraction
            })
    })
}

fn segment_triangle_fraction(origin: Vec3, direction: Vec3, vertices: [Vec3; 3]) -> Option<f64> {
    let edge1 = vertices[1].sub(vertices[0]);
    let edge2 = vertices[2].sub(vertices[0]);
    let p = direction.cross(edge2);
    let determinant = edge1.dot(p);
    if determinant.abs() < 1.0e-10 {
        return None;
    }
    let inverse = 1.0 / determinant;
    let t = origin.sub(vertices[0]);
    let u = t.dot(p) * inverse;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = t.cross(edge1);
    let v = direction.dot(q) * inverse;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let fraction = edge2.dot(q) * inverse;
    (0.0..=1.0).contains(&fraction).then_some(fraction)
}

#[derive(Clone, Copy)]
struct Point2 {
    x: f64,
    y: f64,
}

fn triangulate_listener_samples(
    samples: &[EchoListenerSample],
    config: EchoExtractorConfig,
) -> Result<Vec<[usize; 3]>, EchoExtractionError> {
    let mut order = (0..samples.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| {
        samples[*left].position_city_enu_m[0]
            .total_cmp(&samples[*right].position_city_enu_m[0])
            .then_with(|| {
                samples[*left].position_city_enu_m[1]
                    .total_cmp(&samples[*right].position_city_enu_m[1])
            })
            .then_with(|| samples[*left].key.cmp(&samples[*right].key))
    });
    for pair in order.windows(2) {
        let left = Vec3::from_f32(samples[pair[0]].position_city_enu_m);
        let right = Vec3::from_f32(samples[pair[1]].position_city_enu_m);
        if ((left.x - right.x).powi(2) + (left.y - right.y).powi(2)).sqrt()
            <= config.weld_tolerance_m
        {
            return Err(EchoExtractionError::UnsupportedListenerTopology(
                "listener samples collapse to the same horizontal point at 1 mm precision"
                    .to_owned(),
            ));
        }
    }
    let mut points = order
        .iter()
        .map(|index| Point2 {
            x: f64::from(samples[*index].position_city_enu_m[0]),
            y: f64::from(samples[*index].position_city_enu_m[1]),
        })
        .collect::<Vec<_>>();
    let min_x = points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let max_x = points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);
    let min_y = points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let max_y = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
    let span = (max_x - min_x).max(max_y - min_y);
    if span <= config.weld_tolerance_m {
        return Err(EchoExtractionError::UnsupportedListenerTopology(
            "listener samples are collinear or coincident".to_owned(),
        ));
    }
    let center = Point2 {
        x: (min_x + max_x) * 0.5,
        y: (min_y + max_y) * 0.5,
    };
    let super_start = points.len();
    points.extend([
        Point2 {
            x: center.x - span * 32.0,
            y: center.y - span * 16.0,
        },
        Point2 {
            x: center.x + span * 32.0,
            y: center.y - span * 16.0,
        },
        Point2 {
            x: center.x,
            y: center.y + span * 32.0,
        },
    ]);
    let mut triangles = vec![[super_start, super_start + 1, super_start + 2]];
    for point_index in 0..order.len() {
        let mut bad = Vec::new();
        for (triangle_index, triangle) in triangles.iter().enumerate() {
            if circumcircle_contains(points[point_index], triangle.map(|index| points[index])) {
                bad.push(triangle_index);
            }
        }
        let mut edges = BTreeMap::<[usize; 2], usize>::new();
        for triangle_index in &bad {
            for edge in triangle_edges(triangles[*triangle_index]) {
                *edges.entry(edge).or_default() += 1;
            }
        }
        let bad = bad.into_iter().collect::<BTreeSet<_>>();
        triangles = triangles
            .into_iter()
            .enumerate()
            .filter_map(|(index, triangle)| (!bad.contains(&index)).then_some(triangle))
            .collect();
        for (edge, count) in edges {
            if count != 1 {
                continue;
            }
            let mut triangle = [edge[0], edge[1], point_index];
            if orientation(
                points[triangle[0]],
                points[triangle[1]],
                points[triangle[2]],
            ) < 0.0
            {
                triangle.swap(0, 1);
            }
            if orientation(
                points[triangle[0]],
                points[triangle[1]],
                points[triangle[2]],
            )
            .abs()
                > 1.0e-10
            {
                triangles.push(triangle);
            }
        }
    }
    triangles.retain(|triangle| triangle.iter().all(|index| *index < order.len()));
    if triangles.is_empty() {
        return Err(EchoExtractionError::UnsupportedListenerTopology(
            "listener samples did not form a 2D triangulation".to_owned(),
        ));
    }
    let mut result = triangles
        .into_iter()
        .map(|triangle| triangle.map(|sorted_index| order[sorted_index]))
        .collect::<Vec<_>>();
    result.sort_by_key(|triangle| {
        let mut keys = triangle.map(|index| samples[index].key);
        keys.sort();
        keys
    });
    Ok(result)
}

fn circumcircle_contains(point: Point2, triangle: [Point2; 3]) -> bool {
    let ax = triangle[0].x - point.x;
    let ay = triangle[0].y - point.y;
    let bx = triangle[1].x - point.x;
    let by = triangle[1].y - point.y;
    let cx = triangle[2].x - point.x;
    let cy = triangle[2].y - point.y;
    let determinant = (ax * ax + ay * ay) * (bx * cy - cx * by)
        - (bx * bx + by * by) * (ax * cy - cx * ay)
        + (cx * cx + cy * cy) * (ax * by - bx * ay);
    let orientation = orientation(triangle[0], triangle[1], triangle[2]);
    if orientation > 0.0 {
        determinant > 1.0e-10
    } else {
        determinant < -1.0e-10
    }
}

fn orientation(a: Point2, b: Point2, c: Point2) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn triangle_edges(vertices: [usize; 3]) -> [[usize; 2]; 3] {
    [
        ordered_edge(vertices[0], vertices[1]),
        ordered_edge(vertices[1], vertices[2]),
        ordered_edge(vertices[2], vertices[0]),
    ]
}

fn ordered_edge(a: usize, b: usize) -> [usize; 2] {
    if a < b { [a, b] } else { [b, a] }
}

fn patch_key(mesh: &WeldedMesh, material_id: u32, triangles: &[usize]) -> StableGeometryKey {
    let mut canonical_triangles = triangles
        .iter()
        .map(|index| {
            let mut vertices = mesh.triangles[*index]
                .vertices
                .map(|vertex| quantized_mm(mesh.vertices[vertex]));
            vertices.sort();
            vertices
        })
        .collect::<Vec<_>>();
    canonical_triangles.sort();
    let mut bytes = material_id.to_le_bytes().to_vec();
    for triangle in canonical_triangles {
        for vertex in triangle {
            for coordinate in vertex {
                bytes.extend_from_slice(&coordinate.to_le_bytes());
            }
        }
    }
    StableGeometryKey::derive("echo-facade-patch-v1", &bytes)
}

fn geometry_edge_bytes(a: Vec3, b: Vec3) -> Vec<u8> {
    let mut endpoints = [quantized_mm(a), quantized_mm(b)];
    endpoints.sort();
    let mut bytes = Vec::with_capacity(48);
    for endpoint in endpoints {
        for coordinate in endpoint {
            bytes.extend_from_slice(&coordinate.to_le_bytes());
        }
    }
    bytes
}

fn material_key(
    materials: &MaterialTable,
    material_id: u32,
) -> Result<StableGeometryKey, EchoExtractionError> {
    let (name, material) = materials.iter().nth(material_id as usize).ok_or_else(|| {
        EchoExtractionError::InvalidMesh(format!("unknown material #{material_id}"))
    })?;
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    for value in material
        .absorption
        .into_iter()
        .chain([material.scattering])
        .chain(material.transmission)
    {
        bytes.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    Ok(StableGeometryKey::derive("echo-material-v1", &bytes))
}

fn edge_material_key(
    materials: &MaterialTable,
    patches: &[FacadePatchInternal],
    edge: &DiffractionEdgeInternal,
) -> Result<StableGeometryKey, EchoExtractionError> {
    let mut keys = edge
        .adjacent_patches
        .iter()
        .map(|index| material_key(materials, patches[*index].material_id))
        .collect::<Result<Vec<_>, _>>()?;
    keys.sort();
    let bytes = keys.into_iter().flat_map(|key| key.0).collect::<Vec<_>>();
    Ok(StableGeometryKey::derive("echo-edge-materials-v1", &bytes))
}

fn path_key(
    anchor: StableSpatialKey,
    geometry: StableGeometryKey,
    kind: EchoPathKind,
) -> StablePathKey {
    let mut bytes = Vec::with_capacity(33);
    bytes.extend_from_slice(&anchor.0);
    bytes.extend_from_slice(&geometry.0);
    bytes.push(kind as u8);
    StablePathKey::derive("echo-mesh-path-v1", &bytes)
}

fn plan_tile_key(
    anchor: StableSpatialKey,
    cell: StableSpatialKey,
    vertices: [StableSpatialKey; 3],
) -> StableSpatialKey {
    let mut sorted = vertices;
    sorted.sort();
    let mut bytes = Vec::with_capacity(80);
    bytes.extend_from_slice(&anchor.0);
    bytes.extend_from_slice(&cell.0);
    for vertex in sorted {
        bytes.extend_from_slice(&vertex.0);
    }
    StableSpatialKey::derive("echo-plan-tile-v1", &bytes)
}

fn hash_anchors(anchors: &[StaticSourceAnchor]) -> Sha256Digest {
    let mut canonical = anchors.to_vec();
    canonical.sort_by_key(|anchor| anchor.key);
    let mut bytes = Vec::new();
    for anchor in canonical {
        bytes.extend_from_slice(&anchor.key.0);
        for value in anchor.position_city_enu_m {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }
    Sha256Digest::from_bytes(&bytes)
}

fn hash_listener_layout(coverage: &[EchoListenerCoverage]) -> Sha256Digest {
    let mut canonical = coverage.to_vec();
    canonical.sort_by_key(|cell| cell.cell);
    let mut bytes = Vec::new();
    for mut cell in canonical {
        bytes.extend_from_slice(&cell.cell.0);
        cell.samples.sort_by_key(|sample| sample.key);
        for sample in cell.samples {
            bytes.extend_from_slice(&sample.key.0);
            for value in sample.position_city_enu_m {
                bytes.extend_from_slice(&value.to_bits().to_le_bytes());
            }
        }
    }
    Sha256Digest::from_bytes(&bytes)
}

fn hash_request(request: &EchoExtractionRequest, config: EchoExtractorConfig) -> Sha256Digest {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&hash_anchors(&request.anchors).0);
    bytes.extend_from_slice(&hash_listener_layout(&request.listener_coverage).0);
    for value in [
        config.weld_tolerance_m,
        config.facade_vertical_tolerance_degrees,
        config.coplanar_normal_tolerance_degrees,
        config.coplanar_distance_tolerance_m,
        config.ray_epsilon_m,
        config.boundary_tolerance_m,
        config.minimum_reflector_area_m2,
        config.minimum_vertical_extent_m,
        config.minimum_in_plane_extent_m,
    ] {
        bytes.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    Sha256Digest::from_bytes(&bytes)
}

fn quantized_cell(point: Vec3, cell_m: f64) -> [i64; 3] {
    [
        (point.x / cell_m).floor() as i64,
        (point.y / cell_m).floor() as i64,
        (point.z / cell_m).floor() as i64,
    ]
}

fn quantized_mm(point: Vec3) -> [i64; 3] {
    [
        (point.x * 1_000.0).round() as i64,
        (point.y * 1_000.0).round() as i64,
        (point.z * 1_000.0).round() as i64,
    ]
}

fn finite_f32(value: [f32; 3]) -> bool {
    value.iter().all(|component| component.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Material;
    use std::collections::BTreeMap;

    fn material_table() -> MaterialTable {
        MaterialTable::new(BTreeMap::from([(
            "concrete".to_owned(),
            Material {
                absorption: [0.02, 0.03, 0.05],
                scattering: 0.1,
                transmission: [0.0; 3],
            },
        )]))
    }

    fn facade_mesh(include_facade: bool) -> AcousticMesh {
        if !include_facade {
            return AcousticMesh {
                vertices_enu_m: vec![],
                triangles: vec![],
                material_ids: vec![],
            };
        }
        AcousticMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, -12.0, 0.0),
                EnuVector3::new(0.0, 12.0, 0.0),
                EnuVector3::new(0.0, 12.0, 12.0),
                EnuVector3::new(0.0, -12.0, 12.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            material_ids: vec![0, 0],
        }
    }

    fn corner_mesh() -> AcousticMesh {
        AcousticMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, 0.0, 0.0),
                EnuVector3::new(0.0, 20.0, 0.0),
                EnuVector3::new(0.0, 20.0, 12.0),
                EnuVector3::new(0.0, 0.0, 12.0),
                EnuVector3::new(20.0, 0.0, 0.0),
                EnuVector3::new(20.0, 0.0, 12.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3], [0, 3, 5], [0, 5, 4]],
            material_ids: vec![0; 4],
        }
    }

    fn request() -> EchoExtractionRequest {
        let anchor = StaticSourceAnchor {
            key: StableSpatialKey::derive("test-anchor", b"a"),
            position_city_enu_m: [70.0, -2.0, 1.5],
        };
        let cell = StableSpatialKey::derive("test-cell", b"c");
        let positions = [
            [70.0, 2.0, 1.5],
            [70.0, 4.0, 1.5],
            [68.0, 2.0, 1.5],
            [68.0, 4.0, 1.5],
        ];
        EchoExtractionRequest {
            bindings: EchoExtractionBindings {
                package_manifest_hash: Sha256Digest::from_bytes(b"package"),
                mesh_hash: Sha256Digest::from_bytes(b"mesh"),
                material_hash: Sha256Digest::from_bytes(b"materials"),
                coordinate_frame: StableSpatialKey::derive("frame", b"city-enu"),
            },
            anchors: vec![anchor],
            listener_coverage: vec![EchoListenerCoverage {
                cell,
                samples: positions
                    .into_iter()
                    .enumerate()
                    .map(|(index, position_city_enu_m)| EchoListenerSample {
                        key: StableSpatialKey::derive("test-listener", &[index as u8]),
                        position_city_enu_m,
                    })
                    .collect(),
            }],
            discarded_polygon_hole_count: 0,
        }
    }

    #[test]
    fn controlled_facade_extracts_stable_patch_and_audible_return() {
        let extractor = EchoExtractor::new(EchoExtractorConfig::default()).unwrap();
        let first = extractor
            .extract(&facade_mesh(true), &material_table(), request())
            .unwrap();
        let second = extractor
            .extract(&facade_mesh(true), &material_table(), request())
            .unwrap();
        assert_eq!(first.table_bytes(), second.table_bytes());
        assert_eq!(first.patches.len(), 1);
        assert!(first.manifest.stats.emitted_candidates > 0);
        let query = crate::EchoAuthorityQuery {
            anchor: first.table.anchors()[0].key,
            source_position_city_enu_m: first.table.anchors()[0].position_city_enu_m,
            listener_cell: first.table.listener_nodes()[0].listener_cell,
            listener_position_city_enu_m: [69.0, 3.0, 1.5],
        };
        let plan = first.table.query(query).unwrap();
        assert_eq!(plan.telemetry, crate::EchoQueryTelemetry::Ready);
        assert!(
            plan.taps
                .iter()
                .any(|tap| tap.kind == EchoPathKind::Specular)
        );
        assert!(plan.taps[0].excess_path_m / 343.0 >= 0.3);
    }

    #[test]
    fn deleted_facade_removes_return_and_discarded_holes_are_rejected() {
        let extractor = EchoExtractor::new(EchoExtractorConfig::default()).unwrap();
        let deleted = extractor
            .extract(&facade_mesh(false), &material_table(), request())
            .unwrap();
        assert!(deleted.patches.is_empty());
        assert_eq!(deleted.manifest.stats.emitted_candidates, 0);

        let mut holes = request();
        holes.discarded_polygon_hole_count = 1;
        assert!(matches!(
            extractor.extract(&facade_mesh(true), &material_table(), holes),
            Err(EchoExtractionError::UnsupportedPolygonHoles { discarded_count: 1 })
        ));
    }

    #[test]
    fn controlled_corner_discovers_shared_vertical_edge_and_nlos_diffraction() {
        let extractor = EchoExtractor::new(EchoExtractorConfig::default()).unwrap();
        let mut corner_request = request();
        corner_request.anchors[0].position_city_enu_m = [-10.0, 10.0, 1.5];
        let positions = [
            [10.0, -8.0, 1.5],
            [12.0, -8.0, 1.5],
            [10.0, -10.0, 1.5],
            [12.0, -10.0, 1.5],
        ];
        for (sample, position) in corner_request.listener_coverage[0]
            .samples
            .iter_mut()
            .zip(positions)
        {
            sample.position_city_enu_m = position;
        }
        let artifact = extractor
            .extract(&corner_mesh(), &material_table(), corner_request)
            .unwrap();
        assert_eq!(artifact.patches.len(), 2);
        assert!(
            artifact
                .diffraction_edges
                .iter()
                .any(|edge| edge.adjacent_patches.len() == 2)
        );
        let plan = artifact
            .table
            .query(crate::EchoAuthorityQuery {
                anchor: artifact.table.anchors()[0].key,
                source_position_city_enu_m: artifact.table.anchors()[0].position_city_enu_m,
                listener_cell: artifact.table.listener_nodes()[0].listener_cell,
                listener_position_city_enu_m: [11.0, -9.0, 1.5],
            })
            .unwrap();
        assert_eq!(plan.telemetry, crate::EchoQueryTelemetry::Ready);
        assert!(
            plan.taps
                .iter()
                .any(|tap| tap.kind == EchoPathKind::Diffraction)
        );
    }
}
