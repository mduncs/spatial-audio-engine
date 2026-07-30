use std::{
    fs,
    path::{Path, PathBuf},
};

use fightbox_api::EnuVector3;
use serde_json::{Map, Value, json};

use crate::{AcousticMesh, Assumption, MaterialTable, Result, WorldError, sha256::sha256_hex};

pub const FORMAT_VERSION: u32 = 1;
const MESH_MAGIC: &[u8; 8] = b"FBXMESH\0";
const MESH_FILE: &str = "mesh.bin";
const MATERIALS_FILE: &str = "materials.json";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Provenance {
    pub path: String,
    pub sha256: String,
}

impl Provenance {
    #[must_use]
    pub fn from_bytes(path: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            path: path.into(),
            sha256: sha256_hex(bytes),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackageManifest {
    pub format_version: u32,
    pub tool_version: String,
    pub inputs: Vec<Provenance>,
    pub vertex_count: usize,
    pub triangle_count: usize,
    pub mesh_content_sha256: String,
    pub materials_content_sha256: String,
    pub materials: MaterialTable,
    pub building_count: usize,
    pub assumptions: Vec<Assumption>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackageMetadata {
    pub building_count: usize,
    pub assumptions: Vec<Assumption>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedPackage {
    pub manifest: PackageManifest,
    pub mesh: AcousticMesh,
    pub materials: MaterialTable,
}

/// Writes a deterministic `.fightbox` directory.
///
/// `mesh.bin` is little-endian: 8-byte `FBXMESH\0` magic; `u32` format,
/// vertex, and triangle counts; vertices as contiguous `(east, north, up)`
/// `f32` triples; triangles as `u32` index triples; then one `u32` material ID
/// per triangle. JSON contains sorted object keys and no timestamps.
pub fn write_package(
    directory: impl AsRef<Path>,
    mesh: &AcousticMesh,
    materials: &MaterialTable,
    inputs: &[Provenance],
    tool_version: &str,
) -> Result<PackageManifest> {
    write_package_with_metadata(
        directory,
        mesh,
        materials,
        inputs,
        tool_version,
        &PackageMetadata::default(),
    )
}

pub fn write_package_with_metadata(
    directory: impl AsRef<Path>,
    mesh: &AcousticMesh,
    materials: &MaterialTable,
    inputs: &[Provenance],
    tool_version: &str,
    metadata: &PackageMetadata,
) -> Result<PackageManifest> {
    materials.validate()?;
    mesh.validate(materials.iter().len(), usize::MAX)?;
    if tool_version.trim().is_empty() {
        return Err(WorldError::InvalidPackage(
            "tool version must not be empty".to_owned(),
        ));
    }
    for input in inputs {
        validate_sha256(&input.sha256, "input provenance")?;
        if input.path.is_empty() {
            return Err(WorldError::InvalidPackage(
                "input provenance path must not be empty".to_owned(),
            ));
        }
    }

    let directory = directory.as_ref();
    fs::create_dir_all(directory).map_err(|error| WorldError::io(directory, error))?;
    let mesh_bytes = encode_mesh(mesh)?;
    let materials_bytes = json_bytes(&materials.to_json())?;
    let mut sorted_inputs = inputs.to_vec();
    sorted_inputs.sort();
    let mut assumptions = metadata.assumptions.clone();
    assumptions.sort_by(|left, right| left.building_id.cmp(&right.building_id));
    let manifest = PackageManifest {
        format_version: FORMAT_VERSION,
        tool_version: tool_version.to_owned(),
        inputs: sorted_inputs,
        vertex_count: mesh.vertices_enu_m.len(),
        triangle_count: mesh.triangles.len(),
        mesh_content_sha256: sha256_hex(&mesh_bytes),
        materials_content_sha256: sha256_hex(&materials_bytes),
        materials: materials.clone(),
        building_count: metadata.building_count,
        assumptions,
    };
    let manifest_bytes = json_bytes(&manifest_to_json(&manifest))?;
    write_file(directory.join(MESH_FILE), &mesh_bytes)?;
    write_file(directory.join(MATERIALS_FILE), &materials_bytes)?;
    write_file(directory.join(MANIFEST_FILE), &manifest_bytes)?;
    Ok(manifest)
}

/// Loads a package and re-verifies both serialized content hashes and mesh counts.
pub fn read_package(directory: impl AsRef<Path>) -> Result<LoadedPackage> {
    let directory = directory.as_ref();
    let manifest_bytes = read_file(directory.join(MANIFEST_FILE))?;
    let manifest_value: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| WorldError::InvalidPackage(format!("manifest JSON: {error}")))?;
    let manifest = manifest_from_json(&manifest_value)?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(WorldError::InvalidPackage(format!(
            "unsupported format version {}",
            manifest.format_version
        )));
    }
    let mesh_bytes = read_file(directory.join(MESH_FILE))?;
    if sha256_hex(&mesh_bytes) != manifest.mesh_content_sha256 {
        return Err(WorldError::HashMismatch { item: "mesh" });
    }
    let materials_bytes = read_file(directory.join(MATERIALS_FILE))?;
    if sha256_hex(&materials_bytes) != manifest.materials_content_sha256 {
        return Err(WorldError::HashMismatch { item: "materials" });
    }
    let materials_value: Value = serde_json::from_slice(&materials_bytes)
        .map_err(|error| WorldError::InvalidPackage(format!("materials JSON: {error}")))?;
    let materials = MaterialTable::from_json(&materials_value)?;
    if materials != manifest.materials {
        return Err(WorldError::InvalidPackage(
            "manifest material table differs from materials.json".to_owned(),
        ));
    }
    let mesh = decode_mesh(&mesh_bytes)?;
    if mesh.vertices_enu_m.len() != manifest.vertex_count
        || mesh.triangles.len() != manifest.triangle_count
    {
        return Err(WorldError::InvalidPackage(
            "mesh counts differ from manifest".to_owned(),
        ));
    }
    mesh.validate(materials.iter().len(), usize::MAX)?;
    Ok(LoadedPackage {
        manifest,
        mesh,
        materials,
    })
}

pub fn mesh_content_hash(mesh: &AcousticMesh) -> Result<String> {
    Ok(sha256_hex(&encode_mesh(mesh)?))
}

fn encode_mesh(mesh: &AcousticMesh) -> Result<Vec<u8>> {
    let vertex_count = u32::try_from(mesh.vertices_enu_m.len())
        .map_err(|_| WorldError::InvalidPackage("too many mesh vertices".to_owned()))?;
    let triangle_count = u32::try_from(mesh.triangles.len())
        .map_err(|_| WorldError::InvalidPackage("too many mesh triangles".to_owned()))?;
    let capacity = 20_usize
        .checked_add(mesh.vertices_enu_m.len().saturating_mul(12))
        .and_then(|size| size.checked_add(mesh.triangles.len().saturating_mul(16)))
        .ok_or_else(|| WorldError::InvalidPackage("mesh binary is too large".to_owned()))?;
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(MESH_MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&vertex_count.to_le_bytes());
    bytes.extend_from_slice(&triangle_count.to_le_bytes());
    for vertex in &mesh.vertices_enu_m {
        bytes.extend_from_slice(&vertex.east_m.to_le_bytes());
        bytes.extend_from_slice(&vertex.north_m.to_le_bytes());
        bytes.extend_from_slice(&vertex.up_m.to_le_bytes());
    }
    for triangle in &mesh.triangles {
        for index in triangle {
            bytes.extend_from_slice(&index.to_le_bytes());
        }
    }
    for material_id in &mesh.material_ids {
        bytes.extend_from_slice(&material_id.to_le_bytes());
    }
    Ok(bytes)
}

fn decode_mesh(bytes: &[u8]) -> Result<AcousticMesh> {
    if bytes.len() < 20 || &bytes[..8] != MESH_MAGIC {
        return Err(WorldError::InvalidPackage(
            "mesh binary has bad or missing magic".to_owned(),
        ));
    }
    let version = read_u32(bytes, 8)?;
    if version != FORMAT_VERSION {
        return Err(WorldError::InvalidPackage(format!(
            "unsupported mesh binary version {version}"
        )));
    }
    let vertex_count = read_u32(bytes, 12)? as usize;
    let triangle_count = read_u32(bytes, 16)? as usize;
    let expected = 20_usize
        .checked_add(vertex_count.checked_mul(12).ok_or_else(|| {
            WorldError::InvalidPackage("mesh vertex byte count overflows".to_owned())
        })?)
        .and_then(|size| {
            triangle_count
                .checked_mul(16)
                .and_then(|tail| size.checked_add(tail))
        })
        .ok_or_else(|| WorldError::InvalidPackage("mesh byte count overflows".to_owned()))?;
    if bytes.len() != expected {
        return Err(WorldError::InvalidPackage(format!(
            "mesh binary length is {}, expected {expected}",
            bytes.len()
        )));
    }
    let mut cursor = 20;
    let mut vertices = Vec::with_capacity(vertex_count);
    for _ in 0..vertex_count {
        let east = read_f32(bytes, cursor)?;
        let north = read_f32(bytes, cursor + 4)?;
        let up = read_f32(bytes, cursor + 8)?;
        vertices.push(EnuVector3::new(east, north, up));
        cursor += 12;
    }
    let mut triangles = Vec::with_capacity(triangle_count);
    for _ in 0..triangle_count {
        triangles.push([
            read_u32(bytes, cursor)?,
            read_u32(bytes, cursor + 4)?,
            read_u32(bytes, cursor + 8)?,
        ]);
        cursor += 12;
    }
    let mut material_ids = Vec::with_capacity(triangle_count);
    for _ in 0..triangle_count {
        material_ids.push(read_u32(bytes, cursor)?);
        cursor += 4;
    }
    Ok(AcousticMesh {
        vertices_enu_m: vertices,
        triangles,
        material_ids,
    })
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| WorldError::InvalidPackage("mesh binary ended unexpectedly".to_owned()))?;
    Ok(u32::from_le_bytes(raw.try_into().expect("four bytes")))
}

fn read_f32(bytes: &[u8], offset: usize) -> Result<f32> {
    Ok(f32::from_bits(read_u32(bytes, offset)?))
}

fn manifest_to_json(manifest: &PackageManifest) -> Value {
    json!({
        "format_version": manifest.format_version,
        "inputs": manifest.inputs.iter().map(|input| {
            json!({"path": input.path, "sha256": input.sha256})
        }).collect::<Vec<_>>(),
        "building_count": manifest.building_count,
        "assumptions": manifest.assumptions.iter().map(|assumption| {
            json!({
                "assumed_height_m": assumption.assumed_height_m,
                "building_id": assumption.building_id,
                "reason": assumption.reason,
            })
        }).collect::<Vec<_>>(),
        "materials": manifest.materials.to_json(),
        "materials_content_sha256": manifest.materials_content_sha256,
        "mesh": {
            "content_sha256": manifest.mesh_content_sha256,
            "triangle_count": manifest.triangle_count,
            "vertex_count": manifest.vertex_count,
        },
        "tool_version": manifest.tool_version,
    })
}

fn manifest_from_json(value: &Value) -> Result<PackageManifest> {
    let manifest_object = object(value, "manifest")?;
    let format_version = unsigned(manifest_object.get("format_version"), "format_version")?;
    let tool_version = string(manifest_object.get("tool_version"), "tool_version")?.to_owned();
    let building_count = manifest_object
        .get("building_count")
        .map(|value| unsigned(Some(value), "building_count"))
        .transpose()?
        .map_or(Ok(0), |value| {
            usize::try_from(value)
                .map_err(|_| WorldError::InvalidPackage("building_count is too large".to_owned()))
        })?;
    let assumption_values = manifest_object
        .get("assumptions")
        .map_or(Ok(&[][..]), |value| {
            value.as_array().map(Vec::as_slice).ok_or_else(|| {
                WorldError::InvalidPackage("assumptions must be an array".to_owned())
            })
        })?;
    let mut assumptions = Vec::with_capacity(assumption_values.len());
    for value in assumption_values {
        let row = object(value, "assumption")?;
        let height = row
            .get("assumed_height_m")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                WorldError::InvalidPackage("assumption assumed_height_m must be numeric".to_owned())
            })? as f32;
        if !height.is_finite() || height <= 0.0 {
            return Err(WorldError::InvalidPackage(
                "assumption height must be finite and positive".to_owned(),
            ));
        }
        assumptions.push(Assumption {
            building_id: string(row.get("building_id"), "assumption building_id")?.to_owned(),
            assumed_height_m: height,
            reason: string(row.get("reason"), "assumption reason")?.to_owned(),
        });
    }
    let input_values = manifest_object
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| WorldError::InvalidPackage("inputs must be an array".to_owned()))?;
    let mut inputs = Vec::with_capacity(input_values.len());
    for value in input_values {
        let input = object(value, "input provenance")?;
        let provenance = Provenance {
            path: string(input.get("path"), "input path")?.to_owned(),
            sha256: string(input.get("sha256"), "input sha256")?.to_owned(),
        };
        validate_sha256(&provenance.sha256, "input provenance")?;
        inputs.push(provenance);
    }
    let materials = MaterialTable::from_json(
        manifest_object
            .get("materials")
            .ok_or_else(|| WorldError::InvalidPackage("missing materials".to_owned()))?,
    )?;
    let materials_content_sha256 = string(
        manifest_object.get("materials_content_sha256"),
        "materials hash",
    )?
    .to_owned();
    validate_sha256(&materials_content_sha256, "materials")?;
    let mesh = object(
        manifest_object
            .get("mesh")
            .ok_or_else(|| WorldError::InvalidPackage("missing mesh metadata".to_owned()))?,
        "mesh metadata",
    )?;
    let mesh_content_sha256 = string(mesh.get("content_sha256"), "mesh hash")?.to_owned();
    validate_sha256(&mesh_content_sha256, "mesh")?;
    let vertex_count = usize::try_from(unsigned(mesh.get("vertex_count"), "vertex_count")?)
        .map_err(|_| WorldError::InvalidPackage("vertex_count is too large".to_owned()))?;
    let triangle_count =
        usize::try_from(unsigned(mesh.get("triangle_count"), "triangle_count")?)
            .map_err(|_| WorldError::InvalidPackage("triangle_count is too large".to_owned()))?;
    Ok(PackageManifest {
        format_version: u32::try_from(format_version)
            .map_err(|_| WorldError::InvalidPackage("format_version is too large".to_owned()))?,
        tool_version,
        inputs,
        vertex_count,
        triangle_count,
        mesh_content_sha256,
        materials_content_sha256,
        materials,
        building_count,
        assumptions,
    })
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| WorldError::InvalidPackage(format!("{label} must be an object")))
}

fn string<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| WorldError::InvalidPackage(format!("{label} must be a string")))
}

fn unsigned(value: Option<&Value>, label: &str) -> Result<u64> {
    value
        .and_then(Value::as_u64)
        .ok_or_else(|| WorldError::InvalidPackage(format!("{label} must be an unsigned integer")))
}

fn validate_sha256(value: &str, item: &'static str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorldError::InvalidPackage(format!(
            "{item} SHA-256 must be 64 hexadecimal characters"
        )));
    }
    Ok(())
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value)
        .map_err(|error| WorldError::InvalidPackage(format!("serialize JSON: {error}")))
}

fn write_file(path: PathBuf, bytes: &[u8]) -> Result<()> {
    fs::write(&path, bytes).map_err(|error| WorldError::io(path, error))
}

fn read_file(path: PathBuf) -> Result<Vec<u8>> {
    fs::read(&path).map_err(|error| WorldError::io(path, error))
}
