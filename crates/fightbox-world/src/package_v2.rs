use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Result, WorldError, sha256::sha256_hex};

pub const WORLD_MANIFEST_V2_FORMAT_VERSION: u32 = 2;
pub const WORLD_MANIFEST_V2_SCHEMA_ID: &str = "fightbox.world-manifest.v2";
pub const WORLD_MANIFEST_V2_SCHEMA_JSON: &str = include_str!("../world-manifest-v2.schema.json");
pub const WGS84_LOCAL_FRAME_ID: &str = "wgs84_ecef_enu";

pub const CELL_PROBE_FOOTPRINT_M: f64 = 585.0;
pub const CELL_STRIDE_M: f64 = 485.0;
pub const CELL_PAIRWISE_OVERLAP_M: f64 = 100.0;
pub const CELL_OWNERSHIP_GUARD_M: f64 = 50.0;
pub const CELL_GEOMETRY_HALO_M: f64 = 600.0;
pub const MOBILE_BAKED_PATH_HORIZON_M: f64 = 600.0;
pub const MOBILE_TARGET_RAW_PROBE_PAYLOAD_BYTES: u64 = 48 * 1024 * 1024;
pub const MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
pub const STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY: &str = "fightbox.steam-audio.probe-batch.v1";

const MESH_SCHEMA_ID: &str = "fightbox.mesh.v1";
const MATERIALS_SCHEMA_ID: &str = "fightbox.material-table.v1";
const EXTENSION_POLICY_MODE: &str = "typed_hash_indexed_sidecars";
const UNKNOWN_CORE_FIELDS_POLICY: &str = "reject";
const UNKNOWN_OPTIONAL_CAPABILITIES_POLICY: &str = "retain_and_report";
const UNKNOWN_REQUIRED_CAPABILITIES_POLICY: &str = "reject";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeodeticOrigin {
    pub datum: String,
    pub local_frame: String,
    pub latitude_degrees: f64,
    pub longitude_degrees: f64,
    pub altitude_m: f64,
}

impl GeodeticOrigin {
    #[must_use]
    pub fn wgs84(latitude_degrees: f64, longitude_degrees: f64, altitude_m: f64) -> Self {
        Self {
            datum: "WGS84".to_owned(),
            local_frame: WGS84_LOCAL_FRAME_ID.to_owned(),
            latitude_degrees,
            longitude_degrees,
            altitude_m,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.datum != "WGS84" {
            return invalid("geodetic origin datum must be WGS84");
        }
        if self.local_frame != WGS84_LOCAL_FRAME_ID {
            return invalid(format!(
                "geodetic origin local frame must be {WGS84_LOCAL_FRAME_ID}"
            ));
        }
        if !self.latitude_degrees.is_finite() || !(-90.0..=90.0).contains(&self.latitude_degrees) {
            return invalid("geodetic origin latitude must be finite and within -90..=90 degrees");
        }
        if !self.longitude_degrees.is_finite() || !(-180.0..180.0).contains(&self.longitude_degrees)
        {
            return invalid(
                "geodetic origin longitude must be finite and within -180..180 degrees",
            );
        }
        if !self.altitude_m.is_finite() {
            return invalid("geodetic origin altitude must be finite");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellGridIndex {
    pub east: i32,
    pub north: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellBounds {
    pub min_enu_m: [f64; 3],
    pub max_enu_m: [f64; 3],
}

impl CellBounds {
    pub fn validate(&self) -> Result<()> {
        if self
            .min_enu_m
            .into_iter()
            .chain(self.max_enu_m)
            .any(|value| !value.is_finite())
        {
            return invalid("cell bounds must be finite");
        }
        if self.min_enu_m[0] >= self.max_enu_m[0]
            || self.min_enu_m[1] >= self.max_enu_m[1]
            || self.min_enu_m[2] > self.max_enu_m[2]
        {
            return invalid(
                "cell bounds must increase in east/north and may only be flat in the up axis",
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellSwitchPlane {
    pub neighbor_cell_id: String,
    pub normal_enu: [f64; 3],
    pub offset_m: f64,
}

impl CellSwitchPlane {
    fn validate(&self, city_id: &str) -> Result<()> {
        validate_cell_id_for_city(&self.neighbor_cell_id, city_id)?;
        if self.normal_enu.into_iter().any(|value| !value.is_finite()) || !self.offset_m.is_finite()
        {
            return invalid("cell switch plane values must be finite");
        }
        let length_squared = self
            .normal_enu
            .into_iter()
            .map(|value| value * value)
            .sum::<f64>();
        if (length_squared - 1.0).abs() > 1.0e-9 || self.normal_enu[2].abs() > 1.0e-12 {
            return invalid("cell switch plane normal must be a horizontal unit ENU vector");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionRequirement {
    Optional,
    Required,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageCompression {
    None,
    Zstd,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityExtension {
    pub capability: String,
    pub requirement: ExtensionRequirement,
    pub path: String,
    pub content_sha256: String,
    pub stored_content_sha256: String,
    pub raw_size_bytes: u64,
    pub stored_size_bytes: u64,
    pub compression: PackageCompression,
}

impl CapabilityExtension {
    #[must_use]
    pub fn uncompressed(
        capability: impl Into<String>,
        requirement: ExtensionRequirement,
        path: impl Into<String>,
        bytes: &[u8],
    ) -> Self {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Self {
            capability: capability.into(),
            requirement,
            path: path.into(),
            content_sha256: sha256_hex(bytes),
            stored_content_sha256: sha256_hex(bytes),
            raw_size_bytes: size,
            stored_size_bytes: size,
            compression: PackageCompression::None,
        }
    }

    #[must_use]
    pub fn zstd(
        capability: impl Into<String>,
        requirement: ExtensionRequirement,
        path: impl Into<String>,
        raw_bytes: &[u8],
        stored_bytes: &[u8],
    ) -> Self {
        Self {
            capability: capability.into(),
            requirement,
            path: path.into(),
            content_sha256: sha256_hex(raw_bytes),
            stored_content_sha256: sha256_hex(stored_bytes),
            raw_size_bytes: u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
            stored_size_bytes: u64::try_from(stored_bytes.len()).unwrap_or(u64::MAX),
            compression: PackageCompression::Zstd,
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_capability_id(&self.capability)?;
        validate_sidecar_path(&self.path)?;
        validate_lowercase_sha256(&self.content_sha256, "extension")?;
        validate_lowercase_sha256(&self.stored_content_sha256, "stored extension")?;
        if self.stored_size_bytes == 0 || self.raw_size_bytes == 0 {
            return invalid("extension raw and stored sizes must be positive");
        }
        if self.compression == PackageCompression::None
            && (self.raw_size_bytes != self.stored_size_bytes
                || self.content_sha256 != self.stored_content_sha256)
        {
            return invalid(
                "an uncompressed extension must have equal raw/stored sizes and hashes",
            );
        }
        if self.capability == STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY
            && self.raw_size_bytes > MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES
        {
            return invalid(format!(
                "raw probe payload is {} bytes, above the {}-byte mobile hard limit",
                self.raw_size_bytes, MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorldPackageV2Metadata {
    pub city_id: String,
    pub geodetic_origin: GeodeticOrigin,
    pub cell_grid_index: CellGridIndex,
    pub local_to_city_enu_m: [f64; 3],
    pub bounds_local_enu_m: CellBounds,
    pub neighbors: Vec<String>,
    pub switch_planes: Vec<CellSwitchPlane>,
    pub routes: Vec<String>,
}

impl WorldPackageV2Metadata {
    #[must_use]
    pub fn mobile_cell(
        city_id: impl Into<String>,
        geodetic_origin: GeodeticOrigin,
        cell_grid_index: CellGridIndex,
        bounds_local_enu_m: CellBounds,
    ) -> Self {
        Self {
            city_id: city_id.into(),
            geodetic_origin,
            cell_grid_index,
            local_to_city_enu_m: canonical_cell_translation(cell_grid_index),
            bounds_local_enu_m,
            neighbors: Vec::new(),
            switch_planes: Vec::new(),
            routes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldPackageV2Index {
    pub city: CityIdentity,
    pub cell: WorldCellIndex,
    pub mobile_cell_policy: MobileCellPolicy,
    pub extension_policy: WorldManifestExtensionPolicy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityIdentity {
    pub id: String,
    pub geodetic_origin: GeodeticOrigin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldCellIndex {
    pub id: String,
    pub grid_index: CellGridIndex,
    pub local_to_city_enu_m: [f64; 3],
    pub bounds_local_enu_m: CellBounds,
    pub neighbors: Vec<String>,
    pub switch_planes: Vec<CellSwitchPlane>,
    pub routes: Vec<String>,
    pub supported_ranges: CellSupportedRanges,
    pub payloads: CellPayloadIndex,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellSupportedRanges {
    pub probe_footprint_m: f64,
    pub geometry_halo_m: f64,
    pub baked_path_horizon_m: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellPayloadIndex {
    pub mesh: CorePayloadReference,
    pub materials: CorePayloadReference,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorePayloadReference {
    pub schema: String,
    pub path: String,
    pub content_sha256: String,
    pub stored_content_sha256: String,
    pub raw_size_bytes: u64,
    pub stored_size_bytes: u64,
    pub compression: PackageCompression,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MobileCellPolicy {
    pub stride_m: f64,
    pub pairwise_overlap_m: f64,
    pub ownership_guard_m: f64,
    pub maximum_prepared_neighbors: u32,
    pub maximum_resident_worlds: u32,
    pub target_raw_probe_payload_bytes: u64,
    pub hard_raw_probe_payload_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldManifestExtensionPolicy {
    pub mode: String,
    pub unknown_core_fields: String,
    pub unknown_optional_capabilities: String,
    pub unknown_required_capabilities: String,
}

#[must_use]
pub fn stable_cell_id(city_id: &str, grid_index: CellGridIndex) -> String {
    format!("{city_id}:e{}:n{}", grid_index.east, grid_index.north)
}

pub(crate) fn build_world_index(
    metadata: &WorldPackageV2Metadata,
    mesh_sha256: &str,
    mesh_size_bytes: u64,
    materials_sha256: &str,
    materials_size_bytes: u64,
) -> Result<WorldPackageV2Index> {
    let mut neighbors = metadata.neighbors.clone();
    neighbors.sort();
    let mut switch_planes = metadata.switch_planes.clone();
    switch_planes.sort_by(|left, right| left.neighbor_cell_id.cmp(&right.neighbor_cell_id));
    let mut routes = metadata.routes.clone();
    routes.sort();
    let index = WorldPackageV2Index {
        city: CityIdentity {
            id: metadata.city_id.clone(),
            geodetic_origin: metadata.geodetic_origin.clone(),
        },
        cell: WorldCellIndex {
            id: stable_cell_id(&metadata.city_id, metadata.cell_grid_index),
            grid_index: metadata.cell_grid_index,
            local_to_city_enu_m: metadata.local_to_city_enu_m,
            bounds_local_enu_m: metadata.bounds_local_enu_m.clone(),
            neighbors,
            switch_planes,
            routes,
            supported_ranges: CellSupportedRanges {
                probe_footprint_m: CELL_PROBE_FOOTPRINT_M,
                geometry_halo_m: CELL_GEOMETRY_HALO_M,
                baked_path_horizon_m: MOBILE_BAKED_PATH_HORIZON_M,
            },
            payloads: CellPayloadIndex {
                mesh: core_payload(MESH_SCHEMA_ID, "mesh.bin", mesh_sha256, mesh_size_bytes),
                materials: core_payload(
                    MATERIALS_SCHEMA_ID,
                    "materials.json",
                    materials_sha256,
                    materials_size_bytes,
                ),
            },
        },
        mobile_cell_policy: MobileCellPolicy {
            stride_m: CELL_STRIDE_M,
            pairwise_overlap_m: CELL_PAIRWISE_OVERLAP_M,
            ownership_guard_m: CELL_OWNERSHIP_GUARD_M,
            maximum_prepared_neighbors: 1,
            maximum_resident_worlds: 2,
            target_raw_probe_payload_bytes: MOBILE_TARGET_RAW_PROBE_PAYLOAD_BYTES,
            hard_raw_probe_payload_bytes: MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES,
        },
        extension_policy: WorldManifestExtensionPolicy {
            mode: EXTENSION_POLICY_MODE.to_owned(),
            unknown_core_fields: UNKNOWN_CORE_FIELDS_POLICY.to_owned(),
            unknown_optional_capabilities: UNKNOWN_OPTIONAL_CAPABILITIES_POLICY.to_owned(),
            unknown_required_capabilities: UNKNOWN_REQUIRED_CAPABILITIES_POLICY.to_owned(),
        },
    };
    validate_world_index(
        &index,
        mesh_sha256,
        mesh_size_bytes,
        materials_sha256,
        materials_size_bytes,
    )?;
    Ok(index)
}

pub(crate) fn validate_world_index(
    index: &WorldPackageV2Index,
    mesh_sha256: &str,
    mesh_size_bytes: u64,
    materials_sha256: &str,
    materials_size_bytes: u64,
) -> Result<()> {
    validate_city_id(&index.city.id)?;
    index.city.geodetic_origin.validate()?;
    if index.cell.id != stable_cell_id(&index.city.id, index.cell.grid_index) {
        return invalid("cell ID does not match its city ID and grid index");
    }
    let expected_translation = canonical_cell_translation(index.cell.grid_index);
    for (axis, (actual, expected)) in index
        .cell
        .local_to_city_enu_m
        .into_iter()
        .zip(expected_translation)
        .enumerate()
    {
        require_f64(
            actual,
            expected,
            &format!("cell local-to-city ENU axis {axis}"),
        )?;
    }
    index.cell.bounds_local_enu_m.validate()?;
    validate_sorted_unique(&index.cell.neighbors, "cell neighbors")?;
    for neighbor in &index.cell.neighbors {
        validate_cell_id_for_city(neighbor, &index.city.id)?;
        if neighbor == &index.cell.id {
            return invalid("a cell must not list itself as a neighbor");
        }
    }
    if index
        .cell
        .switch_planes
        .windows(2)
        .any(|pair| pair[0].neighbor_cell_id.as_str() >= pair[1].neighbor_cell_id.as_str())
    {
        return invalid("cell switch planes must be uniquely sorted by neighbor cell ID");
    }
    for plane in &index.cell.switch_planes {
        plane.validate(&index.city.id)?;
    }
    let neighbor_ids = index
        .cell
        .neighbors
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let plane_ids = index
        .cell
        .switch_planes
        .iter()
        .map(|plane| plane.neighbor_cell_id.as_str())
        .collect::<BTreeSet<_>>();
    if neighbor_ids != plane_ids {
        return invalid("cell neighbors and switch-plane neighbor IDs must match exactly");
    }
    validate_sorted_unique(&index.cell.routes, "cell routes")?;
    for route in &index.cell.routes {
        validate_route_id(route)?;
    }
    require_f64(
        index.cell.supported_ranges.probe_footprint_m,
        CELL_PROBE_FOOTPRINT_M,
        "cell probe footprint",
    )?;
    require_f64(
        index.cell.supported_ranges.geometry_halo_m,
        CELL_GEOMETRY_HALO_M,
        "cell geometry halo",
    )?;
    require_f64(
        index.cell.supported_ranges.baked_path_horizon_m,
        MOBILE_BAKED_PATH_HORIZON_M,
        "mobile baked path horizon",
    )?;
    validate_core_payload(
        &index.cell.payloads.mesh,
        MESH_SCHEMA_ID,
        "mesh.bin",
        mesh_sha256,
        mesh_size_bytes,
    )?;
    validate_core_payload(
        &index.cell.payloads.materials,
        MATERIALS_SCHEMA_ID,
        "materials.json",
        materials_sha256,
        materials_size_bytes,
    )?;
    validate_mobile_policy(&index.mobile_cell_policy)?;
    validate_extension_policy(&index.extension_policy)?;
    Ok(())
}

pub(crate) fn canonical_extensions(
    extensions: &[CapabilityExtension],
) -> Result<Vec<CapabilityExtension>> {
    let mut extensions = extensions.to_vec();
    extensions.sort_by(|left, right| {
        (&left.capability, &left.path).cmp(&(&right.capability, &right.path))
    });
    validate_extensions(&extensions)?;
    Ok(extensions)
}

pub(crate) fn validate_extensions(extensions: &[CapabilityExtension]) -> Result<()> {
    let mut identities = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut previous = None;
    let mut raw_probe_payload_bytes = 0_u64;
    for extension in extensions {
        extension.validate()?;
        let identity = (extension.capability.as_str(), extension.path.as_str());
        if previous.is_some_and(|previous| previous >= identity) {
            return invalid("extensions must be uniquely sorted by capability and path");
        }
        previous = Some(identity);
        if !identities.insert(identity) || !paths.insert(extension.path.as_str()) {
            return invalid("extension identities and sidecar paths must be unique");
        }
        if extension.capability == STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY {
            raw_probe_payload_bytes = raw_probe_payload_bytes
                .checked_add(extension.raw_size_bytes)
                .ok_or_else(|| {
                    WorldError::InvalidPackage("raw probe payload size overflows".to_owned())
                })?;
        }
    }
    if raw_probe_payload_bytes > MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES {
        return invalid(format!(
            "raw probe payload is {raw_probe_payload_bytes} bytes, above the {}-byte mobile hard limit",
            MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES
        ));
    }
    Ok(())
}

pub(crate) fn validate_v2_core_json_shape(value: &Value) -> Result<()> {
    let root = value
        .as_object()
        .ok_or_else(|| WorldError::InvalidPackage("manifest must be an object".to_owned()))?;
    exact_keys(
        root,
        &[
            "assumptions",
            "building_count",
            "extensions",
            "format_version",
            "inputs",
            "materials",
            "materials_content_sha256",
            "mesh",
            "schema_version",
            "tool_version",
            "world",
        ],
        "v2 manifest",
    )?;
    let inputs = root["inputs"]
        .as_array()
        .ok_or_else(|| WorldError::InvalidPackage("inputs must be an array".to_owned()))?;
    for input in inputs {
        exact_keys(
            object(input, "input provenance")?,
            &["path", "sha256"],
            "input provenance",
        )?;
    }
    let assumptions = root["assumptions"]
        .as_array()
        .ok_or_else(|| WorldError::InvalidPackage("assumptions must be an array".to_owned()))?;
    for assumption in assumptions {
        exact_keys(
            object(assumption, "assumption")?,
            &["assumed_height_m", "building_id", "reason"],
            "assumption",
        )?;
    }
    exact_keys(
        object(&root["mesh"], "mesh metadata")?,
        &["content_sha256", "triangle_count", "vertex_count"],
        "mesh metadata",
    )?;
    let materials = object(&root["materials"], "materials")?;
    for (name, material) in materials {
        exact_keys(
            object(material, &format!("material {name:?}"))?,
            &["absorption", "scattering", "transmission"],
            &format!("material {name:?}"),
        )?;
    }
    Ok(())
}

pub(crate) fn world_index_from_json(value: &Value) -> Result<WorldPackageV2Index> {
    serde_json::from_value(value.clone())
        .map_err(|error| WorldError::InvalidPackage(format!("v2 world index: {error}")))
}

pub(crate) fn extensions_from_json(value: &Value) -> Result<Vec<CapabilityExtension>> {
    let extensions: Vec<CapabilityExtension> = serde_json::from_value(value.clone())
        .map_err(|error| WorldError::InvalidPackage(format!("v2 extensions: {error}")))?;
    validate_extensions(&extensions)?;
    Ok(extensions)
}

pub(crate) fn value_from_serializable(value: &impl Serialize, label: &str) -> Result<Value> {
    serde_json::to_value(value)
        .map_err(|error| WorldError::InvalidPackage(format!("serialize {label}: {error}")))
}

fn core_payload(
    schema: &str,
    path: &str,
    content_sha256: &str,
    size_bytes: u64,
) -> CorePayloadReference {
    CorePayloadReference {
        schema: schema.to_owned(),
        path: path.to_owned(),
        content_sha256: content_sha256.to_owned(),
        stored_content_sha256: content_sha256.to_owned(),
        raw_size_bytes: size_bytes,
        stored_size_bytes: size_bytes,
        compression: PackageCompression::None,
    }
}

fn validate_core_payload(
    payload: &CorePayloadReference,
    schema: &str,
    path: &str,
    sha256: &str,
    size_bytes: u64,
) -> Result<()> {
    if payload.schema != schema
        || payload.path != path
        || payload.content_sha256 != sha256
        || payload.stored_content_sha256 != sha256
        || payload.raw_size_bytes != size_bytes
        || payload.stored_size_bytes != size_bytes
        || payload.compression != PackageCompression::None
    {
        return invalid("v2 core payload index does not match the package payload bytes");
    }
    Ok(())
}

fn validate_mobile_policy(policy: &MobileCellPolicy) -> Result<()> {
    require_f64(policy.stride_m, CELL_STRIDE_M, "mobile cell stride")?;
    require_f64(
        policy.pairwise_overlap_m,
        CELL_PAIRWISE_OVERLAP_M,
        "mobile cell overlap",
    )?;
    require_f64(
        policy.ownership_guard_m,
        CELL_OWNERSHIP_GUARD_M,
        "mobile cell ownership guard",
    )?;
    if policy.maximum_prepared_neighbors != 1
        || policy.maximum_resident_worlds != 2
        || policy.target_raw_probe_payload_bytes != MOBILE_TARGET_RAW_PROBE_PAYLOAD_BYTES
        || policy.hard_raw_probe_payload_bytes != MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES
    {
        return invalid("mobile cell capability limits differ from the v2 contract");
    }
    Ok(())
}

fn validate_extension_policy(policy: &WorldManifestExtensionPolicy) -> Result<()> {
    if policy.mode != EXTENSION_POLICY_MODE
        || policy.unknown_core_fields != UNKNOWN_CORE_FIELDS_POLICY
        || policy.unknown_optional_capabilities != UNKNOWN_OPTIONAL_CAPABILITIES_POLICY
        || policy.unknown_required_capabilities != UNKNOWN_REQUIRED_CAPABILITIES_POLICY
    {
        return invalid("world manifest extension policy differs from the v2 contract");
    }
    Ok(())
}

fn require_f64(actual: f64, expected: f64, label: &str) -> Result<()> {
    if actual.to_bits() != expected.to_bits() {
        return invalid(format!("{label} differs from the v2 contract"));
    }
    Ok(())
}

fn canonical_cell_translation(grid_index: CellGridIndex) -> [f64; 3] {
    [
        f64::from(grid_index.east) * CELL_STRIDE_M,
        f64::from(grid_index.north) * CELL_STRIDE_M,
        0.0,
    ]
}

fn validate_city_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value.as_bytes()[0].is_ascii_lowercase()
        || value.ends_with('-')
    {
        return invalid(
            "city ID must be 1..=64 lowercase kebab-case characters starting with a letter",
        );
    }
    Ok(())
}

fn validate_cell_id_for_city(value: &str, city_id: &str) -> Result<()> {
    let prefix = format!("{city_id}:e");
    let Some((east, north)) = value
        .strip_prefix(&prefix)
        .and_then(|tail| tail.split_once(":n"))
    else {
        return invalid("cell ID must use the canonical city:e<index>:n<index> form");
    };
    let east = east
        .parse::<i32>()
        .map_err(|_| WorldError::InvalidPackage("cell east index is invalid".to_owned()))?;
    let north = north
        .parse::<i32>()
        .map_err(|_| WorldError::InvalidPackage("cell north index is invalid".to_owned()))?;
    if stable_cell_id(city_id, CellGridIndex { east, north }) != value {
        return invalid("cell ID indices are not in canonical decimal form");
    }
    Ok(())
}

fn validate_route_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b'/')
        })
    {
        return invalid("route IDs must be non-empty lowercase stable identifiers");
    }
    Ok(())
}

pub(crate) fn validate_capability_id(value: &str) -> Result<()> {
    let Some((name, version)) = value.rsplit_once(".v") else {
        return invalid("extension capability must end in .v<positive integer>");
    };
    let parsed_version = version.parse::<u32>().ok().filter(|version| *version > 0);
    if name.is_empty()
        || (!name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit())
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        || parsed_version.is_none()
        || parsed_version.is_some_and(|parsed| parsed.to_string() != version)
    {
        return invalid("extension capability must be a lowercase typed identifier ending in .vN");
    }
    Ok(())
}

fn validate_sidecar_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 240
        || matches!(value, "manifest.json" | "mesh.bin" | "materials.json")
    {
        return invalid("extension sidecar path is empty, too long, or reserved");
    }
    if value.split('/').any(|segment| {
        segment.is_empty()
            || segment.len() > 64
            || segment.starts_with('.')
            || segment.ends_with('.')
            || !segment.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
    }) {
        return invalid(
            "extension sidecar path must be a portable lowercase slash-separated relative path",
        );
    }
    Ok(())
}

fn validate_lowercase_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid(format!(
            "{label} SHA-256 must be 64 lowercase hexadecimal characters"
        ));
    }
    Ok(())
}

fn validate_sorted_unique(values: &[String], label: &str) -> Result<()> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return invalid(format!("{label} must be uniquely sorted"));
    }
    Ok(())
}

fn exact_keys(object: &Map<String, Value>, keys: &[&str], label: &str) -> Result<()> {
    let expected = keys.iter().copied().collect::<BTreeSet<_>>();
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if actual != expected {
        let unknown = actual.difference(&expected).copied().collect::<Vec<_>>();
        let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
        return invalid(format!(
            "{label} fields differ from the strict v2 schema (unknown={unknown:?}, missing={missing:?})"
        ));
    }
    Ok(())
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| WorldError::InvalidPackage(format!("{label} must be an object")))
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(WorldError::InvalidPackage(message.into()))
}
