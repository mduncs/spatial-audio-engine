//! Focused offline CLI for mesh-authoritative static event echo tables.

use std::path::{Path, PathBuf};

use fightbox_world::{
    ECHO_AUTHORITY_CAPABILITY, EchoExtractionBindings, EchoExtractionRequest, EchoExtractor,
    EchoExtractorConfig, EchoListenerCoverage, EchoListenerSample, PackageMetadata, Sha256Digest,
    StableSpatialKey, StaticSourceAnchor, WorldPackageV2Metadata,
    echo_authority_extension_for_package, echo_coordinate_frame_key, echo_listener_cell_key,
    package_manifest_sha256_without_capability, read_package, write_package_v2_with_metadata,
};

use crate::{
    atomicio::{AtomicDir, validate_output_path, write_bytes_atomic, write_json_atomic},
    error::{CliError, Result},
};

#[derive(Debug)]
struct NamedPoint {
    id: String,
    local_enu_m: [f32; 3],
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    let mut package = None;
    let mut output = None;
    let mut package_output = None;
    let mut anchors = Vec::new();
    let mut listeners = Vec::new();
    let mut source_hole_count = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--package" => set_once(&mut package, PathBuf::from(value), flag)?,
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            "--package-output" => set_once(&mut package_output, PathBuf::from(value), flag)?,
            "--anchor" => anchors.push(parse_named_point(value, flag)?),
            "--listener" => listeners.push(parse_named_point(value, flag)?),
            "--source-hole-count" => {
                let count = value.parse::<usize>().map_err(|_| {
                    CliError::new("--source-hole-count requires a non-negative integer")
                })?;
                set_once(&mut source_hole_count, count, flag)?;
            }
            other => {
                return Err(CliError::new(format!(
                    "unknown city echo-authority argument {other:?}"
                )));
            }
        }
    }
    let package = package.ok_or_else(|| CliError::new("missing required --package <path>"))?;
    let output = output.ok_or_else(|| CliError::new("missing required --output <directory>"))?;
    let source_hole_count = source_hole_count.ok_or_else(|| {
        CliError::new(
            "missing required --source-hole-count <n>; current packages do not preserve GeoJSON ring topology, so the extractor requires an explicit topology declaration",
        )
    })?;
    if anchors.is_empty() {
        return Err(CliError::new(
            "at least one --anchor <id:east,north,up> is required",
        ));
    }
    if listeners.len() < 3 {
        return Err(CliError::new(
            "at least three --listener <id:east,north,up> samples are required",
        ));
    }
    bake(
        &package,
        &output,
        package_output.as_deref(),
        anchors,
        listeners,
        source_hole_count,
    )
}

fn bake(
    package: &Path,
    output: &Path,
    package_output: Option<&Path>,
    anchors: Vec<NamedPoint>,
    listeners: Vec<NamedPoint>,
    source_hole_count: usize,
) -> Result<()> {
    let loaded = read_package(package)
        .map_err(|error| CliError::new(format!("cannot read city package: {error}")))?;
    let (cell_name, local_to_city_enu_m, frame_label) = loaded
        .manifest
        .world
        .as_ref()
        .map(|world| {
            (
                world.cell.id.as_str(),
                world.cell.local_to_city_enu_m,
                format!(
                    "{}:{}:{}",
                    world.city.id, world.city.geodetic_origin.local_frame, world.cell.id
                ),
            )
        })
        .unwrap_or(("legacy-cell-0", [0.0; 3], "legacy-local-enu".to_owned()));
    let mut mesh = loaded.mesh.clone();
    for vertex in &mut mesh.vertices_enu_m {
        vertex.east_m += local_to_city_enu_m[0] as f32;
        vertex.north_m += local_to_city_enu_m[1] as f32;
        vertex.up_m += local_to_city_enu_m[2] as f32;
    }
    let to_city = |point: [f32; 3]| {
        std::array::from_fn(|axis| point[axis] + local_to_city_enu_m[axis] as f32)
    };
    let cell = loaded
        .manifest
        .world
        .as_ref()
        .map(|world| echo_listener_cell_key(&world.cell.id))
        .unwrap_or_else(|| echo_listener_cell_key(cell_name));
    let coordinate_frame = loaded
        .manifest
        .world
        .as_ref()
        .map(echo_coordinate_frame_key)
        .unwrap_or_else(|| {
            StableSpatialKey::derive("echo-coordinate-frame-v1", frame_label.as_bytes())
        });
    let base_manifest_sha256 =
        package_manifest_sha256_without_capability(&loaded.manifest, ECHO_AUTHORITY_CAPABILITY)
            .map_err(|error| {
                CliError::new(format!("cannot bind base package manifest: {error}"))
            })?;
    let request = EchoExtractionRequest {
        bindings: EchoExtractionBindings {
            package_manifest_hash: Sha256Digest::from_hex(&base_manifest_sha256)
                .map_err(|error| CliError::new(error.to_string()))?,
            mesh_hash: Sha256Digest::from_hex(&loaded.manifest.mesh_content_sha256)
                .map_err(|error| CliError::new(error.to_string()))?,
            material_hash: Sha256Digest::from_hex(&loaded.manifest.materials_content_sha256)
                .map_err(|error| CliError::new(error.to_string()))?,
            coordinate_frame,
        },
        anchors: anchors
            .into_iter()
            .map(|point| StaticSourceAnchor {
                key: StableSpatialKey::derive("echo-static-anchor-v1", point.id.as_bytes()),
                position_city_enu_m: to_city(point.local_enu_m),
            })
            .collect(),
        listener_coverage: vec![EchoListenerCoverage {
            cell,
            samples: listeners
                .into_iter()
                .map(|point| EchoListenerSample {
                    key: StableSpatialKey::derive(
                        "echo-listener-sample-v1",
                        format!("{cell_name}:{}", point.id).as_bytes(),
                    ),
                    position_city_enu_m: to_city(point.local_enu_m),
                })
                .collect(),
        }],
        discarded_polygon_hole_count: source_hole_count,
    };
    let artifact = EchoExtractor::new(EchoExtractorConfig::default())
        .and_then(|extractor| extractor.extract(&mesh, &loaded.materials, request))
        .map_err(|error| CliError::new(format!("echo authority extraction failed: {error}")))?;
    let table_bytes = artifact.table_bytes();

    let output = validate_output_path(output)?;
    let atomic = AtomicDir::create(output.clone())?;
    write_bytes_atomic(&atomic.temp_path().join("echo-authority.bin"), &table_bytes)?;
    write_json_atomic(
        &atomic.temp_path().join("manifest.json"),
        &artifact.manifest,
    )?;
    atomic.commit()?;
    if let Some(package_output) = package_output {
        write_attached_package(package, package_output, &loaded, &table_bytes)?;
    }
    eprintln!(
        "fightbox: echo authority written to {} (patches={}, edges={}, tiles={}, candidates={}, no-plan={})",
        output.display(),
        artifact.manifest.stats.emitted_facade_patches,
        artifact.manifest.stats.emitted_diffraction_edges,
        artifact.manifest.stats.plan_tiles,
        artifact.manifest.stats.emitted_candidates,
        artifact.manifest.stats.explicit_no_plan_tiles,
    );
    Ok(())
}

fn write_attached_package(
    source_package: &Path,
    output: &Path,
    loaded: &fightbox_world::LoadedPackage,
    table_bytes: &[u8],
) -> Result<()> {
    let world =
        loaded.manifest.world.as_ref().ok_or_else(|| {
            CliError::new("--package-output requires an input world-package-v2 cell")
        })?;
    let extension = echo_authority_extension_for_package(&loaded.manifest, table_bytes)
        .map_err(|error| CliError::new(format!("cannot attach echo authority: {error}")))?;
    let mut extensions = loaded
        .manifest
        .extensions
        .iter()
        .filter(|existing| existing.capability != ECHO_AUTHORITY_CAPABILITY)
        .cloned()
        .collect::<Vec<_>>();
    extensions.push(extension);
    let output = validate_output_path(output)?;
    let atomic = AtomicDir::create(output.clone())?;
    for existing in &loaded.manifest.extensions {
        if existing.capability == ECHO_AUTHORITY_CAPABILITY {
            continue;
        }
        let bytes = std::fs::read(source_package.join(&existing.path)).map_err(|error| {
            CliError::new(format!(
                "cannot copy package extension {}: {error}",
                existing.path
            ))
        })?;
        write_bytes_atomic(&atomic.temp_path().join(&existing.path), &bytes)?;
    }
    write_bytes_atomic(
        &atomic
            .temp_path()
            .join(fightbox_world::ECHO_AUTHORITY_SIDECAR_PATH),
        table_bytes,
    )?;
    let metadata = PackageMetadata {
        building_count: loaded.manifest.building_count,
        assumptions: loaded.manifest.assumptions.clone(),
    };
    let world_metadata = WorldPackageV2Metadata {
        city_id: world.city.id.clone(),
        geodetic_origin: world.city.geodetic_origin.clone(),
        cell_grid_index: world.cell.grid_index,
        local_to_city_enu_m: world.cell.local_to_city_enu_m,
        bounds_local_enu_m: world.cell.bounds_local_enu_m.clone(),
        neighbors: world.cell.neighbors.clone(),
        switch_planes: world.cell.switch_planes.clone(),
        routes: world.cell.routes.clone(),
    };
    write_package_v2_with_metadata(
        atomic.temp_path(),
        &loaded.mesh,
        &loaded.materials,
        &loaded.manifest.inputs,
        &loaded.manifest.tool_version,
        &metadata,
        &world_metadata,
        &extensions,
    )
    .map_err(|error| CliError::new(format!("cannot write extended world package: {error}")))?;
    atomic.commit()?;
    eprintln!(
        "fightbox: extended world package with optional echo authority written to {}",
        output.display()
    );
    Ok(())
}

fn parse_named_point(value: &str, flag: &str) -> Result<NamedPoint> {
    let (id, coordinates) = value
        .split_once(':')
        .ok_or_else(|| CliError::new(format!("{flag} requires <id:east,north,up>")))?;
    if id.trim().is_empty() {
        return Err(CliError::new(format!("{flag} point ID must not be empty")));
    }
    let values = coordinates
        .split(',')
        .map(|value| {
            value
                .parse::<f32>()
                .map_err(|_| CliError::new(format!("{flag} coordinates must be finite numbers")))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.len() != 3 || values.iter().any(|value| !value.is_finite()) {
        return Err(CliError::new(format!(
            "{flag} requires exactly three finite coordinates"
        )));
    }
    Ok(NamedPoint {
        id: id.to_owned(),
        local_enu_m: [values[0], values[1], values[2]],
    })
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        return Err(CliError::new(format!("duplicate {flag}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicU64, Ordering},
    };

    use fightbox_api::EnuVector3;
    use fightbox_world::{
        AcousticMesh, CellBounds, CellGridIndex, GeodeticOrigin, Material, MaterialTable,
        Provenance, WorldPackageV2Metadata, load_package_echo_authority,
        read_package_with_capabilities, write_package_v2,
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fightbox-echo-cli-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn controlled_package_writes_atomic_hash_bound_sidecar() {
        let root = temp("root");
        let package = root.join("world");
        let extended_package = root.join("world-with-echo");
        let output = root.join("echo");
        std::fs::create_dir_all(&root).unwrap();
        let mesh = AcousticMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, -12.0, 0.0),
                EnuVector3::new(0.0, 12.0, 0.0),
                EnuVector3::new(0.0, 12.0, 12.0),
                EnuVector3::new(0.0, -12.0, 12.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            material_ids: vec![0, 0],
        };
        let materials = MaterialTable::new(BTreeMap::from([(
            "concrete".to_owned(),
            Material {
                absorption: [0.02, 0.03, 0.05],
                scattering: 0.1,
                transmission: [0.0; 3],
            },
        )]));
        let world = WorldPackageV2Metadata::mobile_cell(
            "fixture-city",
            GeodeticOrigin::wgs84(0.0, 0.0, 0.0),
            CellGridIndex { east: 0, north: 0 },
            CellBounds {
                min_enu_m: [-1.0, -12.0, 0.0],
                max_enu_m: [1.0, 12.0, 12.0],
            },
        );
        write_package_v2(
            &package,
            &mesh,
            &materials,
            &[Provenance::from_bytes("controlled.obj", b"controlled")],
            "test",
            &world,
            &[],
        )
        .unwrap();
        run(&[
            "--package".into(),
            package.to_string_lossy().into_owned(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
            "--package-output".into(),
            extended_package.to_string_lossy().into_owned(),
            "--source-hole-count".into(),
            "0".into(),
            "--anchor".into(),
            "blast:70,-2,1.5".into(),
            "--listener".into(),
            "a:70,2,1.5".into(),
            "--listener".into(),
            "b:70,4,1.5".into(),
            "--listener".into(),
            "c:68,2,1.5".into(),
            "--listener".into(),
            "d:68,4,1.5".into(),
        ])
        .unwrap();
        let bytes = std::fs::read(output.join("echo-authority.bin")).unwrap();
        assert_eq!(&bytes[..8], fightbox_world::ECHO_AUTHORITY_MAGIC);
        let manifest: fightbox_world::EchoAuthorityArtifactManifest =
            serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest.table_sha256,
            Sha256Digest::from_bytes(&bytes).to_hex()
        );
        assert!(manifest.stats.emitted_candidates > 0);
        let loaded =
            read_package_with_capabilities(&extended_package, &[ECHO_AUTHORITY_CAPABILITY])
                .unwrap();
        let attached = load_package_echo_authority(&extended_package, &loaded)
            .unwrap()
            .unwrap();
        assert_eq!(attached.table.encode(), bytes);
        assert_eq!(attached.cell_id, "fixture-city:e0:n0");
        let _ = std::fs::remove_dir_all(root);
    }
}
