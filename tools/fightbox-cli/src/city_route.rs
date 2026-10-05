//! Deterministic route manifest assembly from already-compiled city cells.
//!
//! This command binds package and optional bake identities. It never launches
//! a probe bake or prepares a runtime world.

use std::path::{Path, PathBuf};

use fightbox_evidence::sha256_hex;
use fightbox_world::{
    CITY_BAKE_V2_CAPABILITY, CITY_BAKE_V2_SIDECAR_PATH, CITY_ROUTE_MANIFEST_FILENAME,
    CityBakeV2BakedArtifact, CityBakeV2ProbePlan, CityRouteAssemblyRequest, CityRouteCellInput,
    CityRouteEchoAuthorityInput, ECHO_AUTHORITY_CAPABILITY, ExtensionRequirement,
    PackageCompression, assemble_city_route, load_package_echo_authority,
    read_package_with_capabilities,
};

use crate::atomicio::{AtomicDir, validate_output_path, write_bytes_atomic};
use crate::bake_reservation::format_bytes;
use crate::error::{CliError, Result};
use crate::probe_byte_estimate_v2::PROBE_BYTE_ESTIMATE_V2_FILENAME;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RouteAssemblyConfig {
    pub route_id: String,
    pub cell_packages: Vec<PathBuf>,
    pub cell_bakes: Vec<PathBuf>,
    pub owner_home_cell_id: Option<String>,
    pub include_four_cell_fixture: bool,
}

pub(crate) fn assemble(config: RouteAssemblyConfig, output: &Path) -> Result<()> {
    if config.cell_packages.is_empty() {
        return Err(CliError::new(
            "city route-assemble requires at least one --cell-package",
        ));
    }
    if !config.cell_bakes.is_empty() && config.cell_bakes.len() != config.cell_packages.len() {
        return Err(CliError::new(
            "--cell-bake must be omitted or supplied once per --cell-package in the same order",
        ));
    }

    let mut cells = Vec::with_capacity(config.cell_packages.len());
    for (index, package) in config.cell_packages.iter().enumerate() {
        cells.push(load_cell(
            package,
            config.cell_bakes.get(index).map(PathBuf::as_path),
            false,
        )?);
    }
    let manifest = assemble_city_route(CityRouteAssemblyRequest {
        route_id: config.route_id,
        cells,
        owner_home_cell_id: config.owner_home_cell_id,
        include_four_cell_fixture: config.include_four_cell_fixture,
    })
    .map_err(|error| CliError::new(format!("cannot assemble city route: {error}")))?;
    let bytes = manifest
        .to_bytes()
        .map_err(|error| CliError::new(format!("cannot serialize city route: {error}")))?;

    let output = validate_output_path(output)?;
    let directory = AtomicDir::create(output.clone())?;
    write_bytes_atomic(
        &directory.temp_path().join(CITY_ROUTE_MANIFEST_FILENAME),
        &bytes,
    )?;
    directory.commit()?;
    eprintln!(
        "fightbox: city route {} written to {} (cells={}, seams={}, completed={}, actual={}, projected-complete={}, fixture-bakes-launched={})",
        manifest.route_id,
        output.display(),
        manifest.cells.len(),
        manifest.adjacencies.len(),
        manifest.installed_totals.completed_cell_count,
        format_bytes(manifest.installed_totals.actual_installed_bytes),
        format_bytes(manifest.installed_totals.projected_complete_installed_bytes),
        manifest
            .four_cell_fixture
            .as_ref()
            .is_some_and(|fixture| fixture.bakes_launched),
    );
    Ok(())
}

pub(crate) fn load_cell(
    package: &Path,
    bake: Option<&Path>,
    allow_legacy_bakes_without_estimate: bool,
) -> Result<CityRouteCellInput> {
    let loaded = read_package_with_capabilities(
        package,
        &[CITY_BAKE_V2_CAPABILITY, ECHO_AUTHORITY_CAPABILITY],
    )
    .map_err(|error| {
        CliError::new(format!(
            "cannot load route cell package {}: {error}",
            package.display()
        ))
    })?;
    let world = loaded.manifest.world.as_ref().ok_or_else(|| {
        CliError::new(format!(
            "route cell {} is not a world-manifest-v2 package",
            package.display()
        ))
    })?;
    let extension = loaded
        .manifest
        .extensions
        .iter()
        .find(|extension| extension.capability == CITY_BAKE_V2_CAPABILITY)
        .ok_or_else(|| {
            CliError::new(format!(
                "route cell {} has no indexed city-bake-v2 plan",
                package.display()
            ))
        })?;
    if extension.path != CITY_BAKE_V2_SIDECAR_PATH
        || extension.requirement != ExtensionRequirement::Required
        || extension.compression != PackageCompression::None
    {
        return Err(CliError::new(format!(
            "route cell plan in {} must be an uncompressed required extension at {CITY_BAKE_V2_SIDECAR_PATH}",
            package.display()
        )));
    }

    let manifest_bytes = read_file(&package.join("manifest.json"), "world manifest")?;
    let plan_bytes = read_file(
        &package.join(CITY_BAKE_V2_SIDECAR_PATH),
        "city-bake-v2 probe plan",
    )?;
    let probe_plan = CityBakeV2ProbePlan::from_sidecar_bytes(&plan_bytes).map_err(|error| {
        CliError::new(format!(
            "invalid probe plan in {}: {error}",
            package.display()
        ))
    })?;
    if probe_plan
        .to_sidecar_bytes()
        .map_err(|error| CliError::new(format!("cannot canonicalize probe plan: {error}")))?
        != plan_bytes
    {
        return Err(CliError::new(format!(
            "route cell probe plan in {} is not canonical planner output",
            package.display()
        )));
    }
    if !probe_plan.byte_estimate.projected_high_within_hard_limit
        || probe_plan.byte_estimate.hard_raw_probe_payload_bytes != 67_108_864
        || probe_plan.byte_estimate.target_raw_probe_payload_bytes != 50_331_648
        || probe_plan.byte_estimate.path_horizon_m != 600
    {
        return Err(CliError::new(format!(
            "route cell probe plan in {} is not mobile-admissible; unrestricted calibration belongs in the oracle lane",
            package.display()
        )));
    }
    if probe_plan.cell.grid_index != world.cell.grid_index
        || probe_plan
            .cell
            .local_to_city_enu_mm
            .map(|millimetres| millimetres as f64 / 1_000.0)
            != world.cell.local_to_city_enu_m
    {
        return Err(CliError::new(format!(
            "route cell package and probe plan disagree in {}",
            package.display()
        )));
    }

    let package_bytes = [
        u64::try_from(manifest_bytes.len())
            .map_err(|_| CliError::new("world manifest size exceeds u64"))?,
        world.cell.payloads.mesh.stored_size_bytes,
        world.cell.payloads.materials.stored_size_bytes,
    ]
    .into_iter()
    .chain(
        loaded
            .manifest
            .extensions
            .iter()
            .map(|extension| extension.stored_size_bytes),
    )
    .try_fold(0_u64, checked_size_sum)?;
    let echo_authority = load_package_echo_authority(package, &loaded)
        .map_err(|error| {
            CliError::new(format!(
                "invalid echo authority in route cell {}: {error}",
                package.display()
            ))
        })?
        .map(|authority| -> Result<CityRouteEchoAuthorityInput> {
            Ok(CityRouteEchoAuthorityInput {
                content_sha256: authority.content_sha256,
                serialized_size_bytes: authority.serialized_size_bytes,
                resident_size_bytes: authority.resident_size_bytes,
                anchor_set_sha256: authority.table.bindings().anchor_set_hash.to_hex(),
                listener_layout_sha256: authority.table.bindings().probe_layout_hash.to_hex(),
                coordinate_frame_key: authority.table.bindings().coordinate_frame.to_hex(),
                static_anchor_count: u32::try_from(authority.table.anchors().len())
                    .map_err(|_| CliError::new("echo authority anchor count exceeds u32"))?,
            })
        })
        .transpose()?;

    let (
        completed_bake,
        completed_bake_sidecar_sha256,
        probe_byte_estimate_v2_sha256,
        probe_byte_estimate_v2_size_bytes,
        installed_bake_bytes,
    ) = if let Some(bake) = bake {
        let sidecar_path = bake.join(CITY_BAKE_V2_SIDECAR_PATH);
        let sidecar_bytes = read_file(&sidecar_path, "completed city-bake-v2 sidecar")?;
        let artifact =
            CityBakeV2BakedArtifact::from_sidecar_bytes(&sidecar_bytes).map_err(|error| {
                CliError::new(format!(
                    "invalid completed bake sidecar {}: {error}",
                    sidecar_path.display()
                ))
            })?;
        if artifact.to_sidecar_bytes().map_err(|error| {
            CliError::new(format!("cannot canonicalize completed bake: {error}"))
        })? != sidecar_bytes
        {
            return Err(CliError::new(format!(
                "completed city bake in {} is not canonical",
                bake.display()
            )));
        }
        let batch_path = bake.join(&artifact.probe_batch.payload_path);
        let batch_bytes = read_file(&batch_path, "probe batch")?;
        let batch_sha256 = sha256_hex(&batch_bytes);
        if u64::try_from(batch_bytes.len()).ok() != Some(artifact.probe_batch.serialized_size_bytes)
            || batch_sha256 != artifact.probe_batch.serialized_sha256
        {
            return Err(CliError::new(format!(
                "probe batch {} differs from its completed sidecar",
                batch_path.display()
            )));
        }
        let metadata_bytes = read_file(
            &bake.join("probe-batch-metadata.json"),
            "probe batch metadata",
        )?;
        let estimate_bytes = read_optional_file(&bake.join(PROBE_BYTE_ESTIMATE_V2_FILENAME))?;
        if estimate_bytes.is_none() && !allow_legacy_bakes_without_estimate {
            return Err(CliError::new(format!(
                "completed city bake {} lacks the required additive probe-byte-estimate-v2 envelope; frozen pre-envelope evidence is admitted only by the legacy oracle verification path",
                bake.display()
            )));
        }
        crate::city_bake_v2::validate_city_bake_artifact_layout(bake, estimate_bytes.is_some())?;
        let batch_len = batch_bytes.len();
        if estimate_bytes.is_some() {
            // The probe batch was already read and hashed from its on-disk
            // location above; when the sidecar declares the canonical
            // "probe-batch.bin" payload path those exact same-location bytes are
            // handed to the verifier instead of re-reading and re-hashing the
            // same immutable file.  Any other declared path keeps the
            // verifier's own read so bytes are always checked where they live.
            let preverified = (artifact.probe_batch.payload_path == "probe-batch.bin").then(|| {
                crate::city_bake_v2::PreverifiedProbeBatch {
                    bytes: batch_bytes,
                    sha256_hex: batch_sha256,
                }
            });
            crate::city_bake_v2::verify_probe_byte_estimate_preverified(
                bake,
                package,
                preverified,
            )?;
        }
        let installed = [
            sidecar_bytes.len(),
            batch_len,
            metadata_bytes.len(),
            estimate_bytes.as_ref().map_or(0, Vec::len),
        ]
        .into_iter()
        .map(|bytes| {
            u64::try_from(bytes).map_err(|_| CliError::new("bake artifact size exceeds u64"))
        })
        .try_fold(0_u64, |total, bytes| checked_size_sum(total, bytes?))?;
        let estimate_identity = estimate_bytes.as_ref().map(|bytes| {
            (
                sha256_hex(bytes),
                u64::try_from(bytes.len()).expect("estimate length was already checked"),
            )
        });
        (
            Some(artifact),
            Some(sha256_hex(&sidecar_bytes)),
            estimate_identity
                .as_ref()
                .map(|identity| identity.0.clone()),
            estimate_identity.map(|identity| identity.1),
            installed,
        )
    } else {
        (None, None, None, None, 0)
    };

    Ok(CityRouteCellInput {
        city_id: world.city.id.clone(),
        cell_id: world.cell.id.clone(),
        grid_index: world.cell.grid_index,
        local_to_city_enu_m: world.cell.local_to_city_enu_m,
        world_manifest_sha256: sha256_hex(&manifest_bytes),
        mesh_sha256: loaded.manifest.mesh_content_sha256.clone(),
        materials_sha256: loaded.manifest.materials_content_sha256.clone(),
        probe_plan_sidecar_sha256: sha256_hex(&plan_bytes),
        probe_plan,
        completed_bake_sidecar_sha256,
        completed_bake,
        probe_byte_estimate_v2_sha256,
        probe_byte_estimate_v2_size_bytes,
        echo_authority,
        installed_package_bytes: package_bytes,
        installed_bake_bytes,
    })
}

fn checked_size_sum(total: u64, next: u64) -> Result<u64> {
    total
        .checked_add(next)
        .ok_or_else(|| CliError::new("city route installed size exceeds u64"))
}

fn read_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|error| CliError::new(format!("cannot read {label} {}: {error}", path.display())))
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliError::new(format!(
            "cannot read optional bake metadata {}: {error}",
            path.display()
        ))),
    }
}
