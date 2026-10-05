//! World-package-v2 attachment and loading for per-cell echo authority.

use std::path::Path;

use crate::{
    CapabilityExtension, CellProbeSlice, EchoAuthorityTable, ExtensionRequirement, LoadedPackage,
    MOBILE_ECHO_AUTHORITY_CAP_BYTES, PackageCompression, PackageManifest, Result, Sha256Digest,
    StableSpatialKey, WORLD_MANIFEST_V2_FORMAT_VERSION, WorldError, WorldPackageV2Index,
    package_manifest_sha256_without_capability,
};

pub const ECHO_AUTHORITY_CAPABILITY: &str = "fightbox.echo-authority.v1";
pub const ECHO_AUTHORITY_SIDECAR_PATH: &str = "echo-authority.bin";

#[derive(Clone, Debug, PartialEq)]
pub struct PackageEchoAuthority {
    pub table: EchoAuthorityTable,
    pub content_sha256: String,
    pub serialized_size_bytes: u64,
    pub resident_size_bytes: u64,
    pub cell_id: String,
}

#[must_use]
pub fn echo_listener_cell_key(cell_id: &str) -> StableSpatialKey {
    StableSpatialKey::derive("echo-listener-cell-v1", cell_id.as_bytes())
}

#[must_use]
pub fn echo_coordinate_frame_key(world: &WorldPackageV2Index) -> StableSpatialKey {
    StableSpatialKey::derive(
        "echo-coordinate-frame-v1",
        format!(
            "{}:{}:{}",
            world.city.id, world.city.geodetic_origin.local_frame, world.cell.id
        )
        .as_bytes(),
    )
}

/// Builds the deterministic optional extension descriptor after validating the
/// table against the package's extension-free base identity.
pub fn echo_authority_extension_for_package(
    package: &PackageManifest,
    table_bytes: &[u8],
) -> Result<CapabilityExtension> {
    let table = EchoAuthorityTable::decode(table_bytes)
        .map_err(|error| invalid_error(format!("echo authority binary: {error}")))?;
    validate_table_binding(package, &table, table_bytes.len())?;
    let extension = CapabilityExtension::uncompressed(
        ECHO_AUTHORITY_CAPABILITY,
        ExtensionRequirement::Optional,
        ECHO_AUTHORITY_SIDECAR_PATH,
        table_bytes,
    );
    extension.validate()?;
    Ok(extension)
}

/// Loads and identity-checks one optional per-cell authority. Absence is the
/// structural Echo-Off state and is not an error.
pub fn load_package_echo_authority(
    package_directory: impl AsRef<Path>,
    package: &LoadedPackage,
) -> Result<Option<PackageEchoAuthority>> {
    let Some(extension) = package
        .manifest
        .extensions
        .iter()
        .find(|extension| extension.capability == ECHO_AUTHORITY_CAPABILITY)
    else {
        return Ok(None);
    };
    if extension.requirement != ExtensionRequirement::Optional
        || extension.path != ECHO_AUTHORITY_SIDECAR_PATH
        || extension.compression != PackageCompression::None
    {
        return invalid(
            "echo authority must be an uncompressed optional extension at echo-authority.bin",
        );
    }
    let path = package_directory.as_ref().join(&extension.path);
    let bytes = std::fs::read(&path).map_err(|error| WorldError::Io {
        path: path.clone(),
        source: error,
    })?;
    if u64::try_from(bytes.len()).ok() != Some(extension.stored_size_bytes) {
        return invalid("echo authority size differs from its extension index");
    }
    let table = EchoAuthorityTable::decode(&bytes)
        .map_err(|error| invalid_error(format!("echo authority binary: {error}")))?;
    validate_table_binding(&package.manifest, &table, bytes.len())?;
    let world = package
        .manifest
        .world
        .as_ref()
        .expect("binding validation requires world-package-v2");
    Ok(Some(PackageEchoAuthority {
        resident_size_bytes: u64::try_from(table.resident_bytes())
            .map_err(|_| invalid_error("echo authority resident size exceeds u64"))?,
        table,
        content_sha256: extension.content_sha256.clone(),
        serialized_size_bytes: extension.raw_size_bytes,
        cell_id: world.cell.id.clone(),
    }))
}

fn validate_table_binding(
    package: &PackageManifest,
    table: &EchoAuthorityTable,
    serialized_size: usize,
) -> Result<()> {
    if package.format_version != WORLD_MANIFEST_V2_FORMAT_VERSION {
        return invalid("echo authority extensions require world-package-v2");
    }
    let world = package
        .world
        .as_ref()
        .ok_or_else(|| invalid_error("echo authority package has no world index"))?;
    if serialized_size > MOBILE_ECHO_AUTHORITY_CAP_BYTES
        || table.resident_bytes() > MOBILE_ECHO_AUTHORITY_CAP_BYTES
    {
        return invalid(format!(
            "per-cell echo authority exceeds the {}-byte mobile admission cap",
            MOBILE_ECHO_AUTHORITY_CAP_BYTES
        ));
    }
    let expected_base_manifest = Sha256Digest::from_hex(
        &package_manifest_sha256_without_capability(package, ECHO_AUTHORITY_CAPABILITY)?,
    )
    .map_err(|error| invalid_error(error.to_string()))?;
    let expected_mesh = Sha256Digest::from_hex(&package.mesh_content_sha256)
        .map_err(|error| invalid_error(error.to_string()))?;
    let expected_materials = Sha256Digest::from_hex(&package.materials_content_sha256)
        .map_err(|error| invalid_error(error.to_string()))?;
    let bindings = table.bindings();
    if bindings.package_manifest_hash != expected_base_manifest
        || bindings.mesh_hash != expected_mesh
        || bindings.material_hash != expected_materials
        || bindings.coordinate_frame != echo_coordinate_frame_key(world)
    {
        return invalid("echo authority identity does not match its world cell package");
    }
    let expected_cell = echo_listener_cell_key(&world.cell.id);
    let footprint =
        CellProbeSlice::for_grid_index(world.cell.grid_index)?.probe_footprint_bounds_city_enu_mm;
    if table.listener_nodes().is_empty()
        || table.listener_nodes().iter().any(|node| {
            node.listener_cell != expected_cell
                || !footprint.contains_closed([
                    (f64::from(node.position_city_enu_m[0]) * 1_000.0).round() as i64,
                    (f64::from(node.position_city_enu_m[1]) * 1_000.0).round() as i64,
                ])
        })
    {
        return invalid("echo authority listener nodes are not bound to this cell footprint");
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(invalid_error(message))
}

fn invalid_error(message: impl Into<String>) -> WorldError {
    WorldError::InvalidPackage(message.into())
}
