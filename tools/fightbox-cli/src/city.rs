//! C2 city CLI: deterministic GeoJSON packages, inspection, Steam Audio
//! probe baking, and package-backed S6a offline capture rendering.

use std::path::{Path, PathBuf};
use std::time::Instant;

use fightbox_api::EnuVector3;
use fightbox_steam_audio::{
    AcousticMaterial, BakedProbeBatch, ElevatedProbeLayer, PROBE_BATCH_METADATA_SCHEMA,
    PathBakeConfig, ProbeBatchMetadata, ProbeMask, ProbeVolume, S3BakeRequest,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SceneMesh,
};
use fightbox_world::{
    AcousticMesh, Assumption, CELL_GEOMETRY_HALO_M, CELL_PROBE_FOOTPRINT_M, CELL_STRIDE_M,
    CITY_BAKE_V2_CAPABILITY, CellBounds, CellGridIndex, CityBoundsMm, CompileOptions,
    GeoJsonOptions, GeoJsonProvider, GeodeticOrigin, GradedProbePolicy, MaterialTable,
    PackageMetadata, Provenance, TriangleProvider, WorldPackageV2Metadata, compile, export_obj,
    read_package, read_package_with_capabilities, write_city_bake_v2_plan_sidecar,
    write_package_v2_with_metadata, write_package_with_metadata,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::atomicio::{
    AtomicDir, validate_output_path, write_bytes_atomic, write_json_atomic,
    write_json_string_atomic,
};
use crate::bake_memory::{BakeGuard, Watchdog, forbid_crash_dumps};
use crate::bake_reservation::{BakeReservation, format_bytes};
use crate::error::{CliError, Result};

const DEFAULT_HEIGHT_M: f32 = 4.0;
const PROBE_SPACING_M: f32 = 4.0;
const PROBE_HEIGHT_M: f32 = 1.5;
const PROBE_CEILING_M: f32 = 3.0;
const EARTH_RADIUS_M: f64 = 6_371_008.8;
const WGS84_SEMI_MAJOR_AXIS_M: f64 = 6_378_137.0;
const WGS84_FIRST_ECCENTRICITY_SQUARED: f64 = 6.694_379_990_141_316_5e-3;
const JITTER_M: f32 = 3.2;
const MIN_ASSUMED_HEIGHT_M: f32 = 3.0;
const OCCLUDER_ID: &str = "way/843220032";
const PERCEPT_SOURCE_M: [f64; 3] = [16.0, -68.0, 1.5];
const PERCEPT_SHADOW_LISTENER_M: [f64; 3] = [-10.0, -68.0, 1.5];
const PERCEPT_LOS_LISTENER_M: [f64; 3] = [16.0, -42.0, 1.5];
const SYNTH_BLOCK_SIZE_M: f64 = 80.0;
const SYNTH_STREET_WIDTH_M: f64 = 15.0;
const BAKE_ARTIFACT_FIXED_BYTES: u64 = 64 * 1024;
const SERIALIZED_BYTES_PER_PROBE: u64 = 256;
const SERIALIZED_BYTES_PER_REACHABLE_PAIR: u64 = 10;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CityCompileV2Config {
    pub city_id: String,
    pub geodetic_origin: GeodeticOrigin,
    pub cell_grid_index: CellGridIndex,
    /// Optional global graded-placement policy. Every cell on a route must use
    /// the same policy bytes so the planner can phase-lock overlap probes.
    pub probe_policy: Option<PathBuf>,
}

pub fn compile_geojson(geojson: &Path, output: &Path) -> Result<()> {
    compile_geojson_input(geojson, output, None)
}

pub(crate) fn compile_osm_geojson(
    geojson: &Path,
    output: &Path,
    projection: crate::city_place::Projection,
) -> Result<()> {
    compile_geojson_input(geojson, output, Some(projection))
}

fn compile_geojson_input(
    geojson: &Path,
    output: &Path,
    projection: Option<crate::city_place::Projection>,
) -> Result<()> {
    let input = std::fs::read(geojson)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", geojson.display())))?;
    let normalized = if let Some(projection) = projection {
        crate::city_place::project_geojson(&input, projection)?
    } else {
        normalize_geojson_coordinates(&input)?
    };
    let provider = GeoJsonProvider::new(
        &normalized,
        GeoJsonOptions {
            default_height_m: Some(DEFAULT_HEIGHT_M),
            ..GeoJsonOptions::default()
        },
    );
    let geometry = provider
        .provide()
        .map_err(|error| CliError::new(format!("cannot compile GeoJSON: {error}")))?;
    // City detail (assessor facades, fences, rail) names materials beyond the
    // default table; every other package keeps the default and its hash.
    let materials = if geometry
        .material_names
        .iter()
        .all(|name| MaterialTable::default().get(name).is_some())
    {
        MaterialTable::default()
    } else {
        MaterialTable::city_detail()
    };
    let mesh = compile(&provider, &materials, CompileOptions::default())
        .map_err(|error| CliError::new(format!("cannot compile acoustic mesh: {error}")))?;
    prepare_package_output(output)?;
    let metadata = PackageMetadata {
        building_count: geometry.building_count,
        assumptions: geometry.assumptions,
    };
    let provenance = [Provenance::from_bytes(geojson.to_string_lossy(), &input)];
    let manifest = write_package_with_metadata(
        output,
        &mesh,
        &materials,
        &provenance,
        env!("CARGO_PKG_VERSION"),
        &metadata,
    )
    .map_err(|error| CliError::new(format!("cannot write package: {error}")))?;
    eprintln!(
        "fightbox: city package written to {} (buildings={}, triangles={}, assumptions={})",
        output.display(),
        manifest.building_count,
        manifest.triangle_count,
        manifest.assumptions.len()
    );
    Ok(())
}

pub fn compile_geojson_v2(
    geojson: &Path,
    output: &Path,
    config: CityCompileV2Config,
) -> Result<()> {
    config
        .geodetic_origin
        .validate()
        .map_err(|error| CliError::new(format!("invalid geodetic origin: {error}")))?;
    let probe_plan = config
        .probe_policy
        .as_deref()
        .map(|path| load_probe_policy_plan(path, config.cell_grid_index))
        .transpose()?;
    let input = std::fs::read(geojson)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", geojson.display())))?;
    let normalized = normalize_geojson_coordinates_at_origin(
        &input,
        &config.geodetic_origin,
        config.cell_grid_index,
    )?;
    let provider = city_provider(&normalized);
    let geometry = provider
        .provide()
        .map_err(|error| CliError::new(format!("cannot compile GeoJSON: {error}")))?;
    let materials = MaterialTable::default();
    let mesh = compile(&provider, &materials, CompileOptions::default())
        .map_err(|error| CliError::new(format!("cannot compile acoustic mesh: {error}")))?;
    let bounds = mesh_bounds(&mesh)?;
    prepare_package_output(output)?;
    let extensions = probe_plan
        .as_ref()
        .map(|plan| {
            write_city_bake_v2_plan_sidecar(output, plan)
                .map(|extension| vec![extension])
                .map_err(|error| {
                    CliError::new(format!("cannot write city-bake-v2 probe plan: {error}"))
                })
        })
        .transpose()?
        .unwrap_or_default();
    let metadata = PackageMetadata {
        building_count: geometry.building_count,
        assumptions: geometry.assumptions,
    };
    let provenance = [Provenance::from_bytes(geojson.to_string_lossy(), &input)];
    let world = WorldPackageV2Metadata::mobile_cell(
        config.city_id,
        config.geodetic_origin,
        config.cell_grid_index,
        bounds,
    );
    let manifest = write_package_v2_with_metadata(
        output,
        &mesh,
        &materials,
        &provenance,
        env!("CARGO_PKG_VERSION"),
        &metadata,
        &world,
        &extensions,
    )
    .map_err(|error| CliError::new(format!("cannot write v2 package: {error}")))?;
    let world = manifest
        .world
        .as_ref()
        .expect("v2 writer returns a world index");
    eprintln!(
        "fightbox: city package v2 written to {} (city={}, cell={}, buildings={}, triangles={}, graded-probes={})",
        output.display(),
        world.city.id,
        world.cell.id,
        manifest.building_count,
        manifest.triangle_count,
        probe_plan.as_ref().map_or(0, |plan| plan.probes.len()),
    );
    Ok(())
}

/// Compiles the monolithic oracle geometry in the authoritative city ENU
/// frame. This writes a v1 acoustic package on purpose: the result is a desktop
/// oracle, not a mobile cell and must not acquire a cell index or phone limits.
pub(crate) fn compile_geojson_oracle(
    geojson: &Path,
    output: &Path,
    geodetic_origin: &GeodeticOrigin,
    geometry_bounds_city_enu_mm: CityBoundsMm,
) -> Result<()> {
    geodetic_origin
        .validate()
        .map_err(|error| CliError::new(format!("invalid geodetic origin: {error}")))?;
    let input = std::fs::read(geojson)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", geojson.display())))?;
    let normalized = normalize_geojson_coordinates_for_oracle(
        &input,
        geodetic_origin,
        geometry_bounds_city_enu_mm,
    )?;
    let provider = city_provider(&normalized);
    let geometry = provider
        .provide()
        .map_err(|error| CliError::new(format!("cannot compile oracle GeoJSON: {error}")))?;
    let materials = MaterialTable::default();
    let mesh = compile(&provider, &materials, CompileOptions::default())
        .map_err(|error| CliError::new(format!("cannot compile oracle acoustic mesh: {error}")))?;
    prepare_package_output(output)?;
    write_package_with_metadata(
        output,
        &mesh,
        &materials,
        &[Provenance::from_bytes(geojson.to_string_lossy(), &input)],
        env!("CARGO_PKG_VERSION"),
        &PackageMetadata {
            building_count: geometry.building_count,
            assumptions: geometry.assumptions,
        },
    )
    .map_err(|error| CliError::new(format!("cannot write oracle package: {error}")))?;
    Ok(())
}

fn load_probe_policy_plan(
    path: &Path,
    grid_index: CellGridIndex,
) -> Result<fightbox_world::CityBakeV2ProbePlan> {
    let bytes = std::fs::read(path).map_err(|error| {
        CliError::new(format!(
            "cannot read graded probe policy {}: {error}",
            path.display()
        ))
    })?;
    let policy: GradedProbePolicy = serde_json::from_slice(&bytes).map_err(|error| {
        CliError::new(format!(
            "invalid graded probe policy {}: {error}",
            path.display()
        ))
    })?;
    policy.plan_cell(grid_index).map_err(|error| {
        CliError::new(format!(
            "cannot resolve graded probe policy {} for cell e{}:n{}: {error}",
            path.display(),
            grid_index.east,
            grid_index.north,
        ))
    })
}

pub fn synth(seed: u64, blocks: (u32, u32), output: &Path) -> Result<()> {
    if output.exists() {
        return Err(CliError::new(format!(
            "GeoJSON output {} already exists",
            output.display()
        )));
    }
    let contents = synth_geojson(seed, blocks)?;
    write_bytes_atomic(output, contents.as_bytes())?;
    eprintln!(
        "fightbox: synthetic city written to {} (seed={}, blocks={}x{})",
        output.display(),
        seed,
        blocks.0,
        blocks.1
    );
    Ok(())
}

fn synth_geojson(seed: u64, blocks: (u32, u32)) -> Result<String> {
    if blocks.0 == 0 || blocks.1 == 0 {
        return Err(CliError::new("--blocks dimensions must both be positive"));
    }
    let block_total = u64::from(blocks.0) * u64::from(blocks.1);
    if block_total > 100_000 {
        return Err(CliError::new(
            "--blocks city is too large (maximum 100000 blocks)",
        ));
    }
    let mut rng = SplitMix64::new(seed);
    let mut features = Vec::new();
    for row in 0..blocks.1 {
        for column in 0..blocks.0 {
            let ordinal = u64::from(row) * u64::from(blocks.0) + u64::from(column);
            let building_count = if ordinal % 9 == 0 {
                2
            } else {
                3 + rng.range_u32(0, 1)
            };
            let block_min = [
                SYNTH_STREET_WIDTH_M
                    + f64::from(column) * (SYNTH_BLOCK_SIZE_M + SYNTH_STREET_WIDTH_M),
                SYNTH_STREET_WIDTH_M + f64::from(row) * (SYNTH_BLOCK_SIZE_M + SYNTH_STREET_WIDTH_M),
            ];
            let cells = building_cells(block_min, building_count, rng.next_u64() & 1 == 0);
            for (index, cell) in cells.into_iter().enumerate() {
                let inset_x = f64::from(rng.range_u32(20, 55)) / 10.0;
                let inset_y = f64::from(rng.range_u32(20, 55)) / 10.0;
                let min = [cell[0] + inset_x, cell[1] + inset_y];
                let max = [cell[2] - inset_x, cell[3] - inset_y];
                let mut properties = std::collections::BTreeMap::new();
                properties.insert(
                    "id".to_owned(),
                    Value::String(format!("synth/{row}/{column}/{index}")),
                );
                if rng.next_u64() & 1 == 0 {
                    properties.insert("height".to_owned(), Value::from(rng.range_u32(4, 60)));
                } else {
                    properties.insert(
                        "building:levels".to_owned(),
                        Value::from(rng.range_u32(2, 18)),
                    );
                }
                features.push(SynthFeature {
                    kind: "Feature",
                    id: format!("synth/{row}/{column}/{index}"),
                    properties,
                    geometry: SynthGeometry {
                        kind: "Polygon",
                        coordinates: vec![vec![
                            [min[0], min[1]],
                            [max[0], min[1]],
                            [max[0], max[1]],
                            [min[0], max[1]],
                            [min[0], min[1]],
                        ]],
                    },
                });
            }
        }
    }
    let collection = SynthFeatureCollection {
        kind: "FeatureCollection",
        features,
    };
    let mut output = serde_json::to_string_pretty(&collection)
        .map_err(|error| CliError::new(format!("cannot encode synthetic GeoJSON: {error}")))?;
    output.push('\n');
    Ok(output)
}

fn building_cells(block_min: [f64; 2], count: u32, rotate: bool) -> Vec<[f64; 4]> {
    let cells = match count {
        2 | 3 => {
            let segment = SYNTH_BLOCK_SIZE_M / f64::from(count);
            (0..count)
                .map(|index| {
                    let start = f64::from(index) * segment;
                    [start, 0.0, start + segment, SYNTH_BLOCK_SIZE_M]
                })
                .collect::<Vec<_>>()
        }
        4 => vec![
            [0.0, 0.0, 40.0, 40.0],
            [40.0, 0.0, 80.0, 40.0],
            [0.0, 40.0, 40.0, 80.0],
            [40.0, 40.0, 80.0, 80.0],
        ],
        _ => unreachable!("synth building count is constrained to 2..=4"),
    };
    cells
        .into_iter()
        .map(|cell| {
            let cell = if rotate {
                [cell[1], cell[0], cell[3], cell[2]]
            } else {
                cell
            };
            [
                block_min[0] + cell[0],
                block_min[1] + cell[1],
                block_min[0] + cell[2],
                block_min[1] + cell[3],
            ]
        })
        .collect()
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn range_u32(&mut self, min: u32, max: u32) -> u32 {
        min + (self.next_u64() % u64::from(max - min + 1)) as u32
    }
}

#[derive(Serialize)]
struct SynthFeatureCollection {
    #[serde(rename = "type")]
    kind: &'static str,
    features: Vec<SynthFeature>,
}

#[derive(Serialize)]
struct SynthFeature {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    properties: std::collections::BTreeMap<String, Value>,
    geometry: SynthGeometry,
}

#[derive(Serialize)]
struct SynthGeometry {
    #[serde(rename = "type")]
    kind: &'static str,
    coordinates: Vec<Vec<[f64; 2]>>,
}

pub fn inspect(package: &Path) -> Result<()> {
    print!("{}", inspect_text(package)?);
    Ok(())
}

pub fn export_package_obj(package: &Path, output: &Path) -> Result<()> {
    if output.exists() {
        return Err(CliError::new(format!(
            "OBJ output {} already exists",
            output.display()
        )));
    }
    let loaded = read_package(package)
        .map_err(|error| CliError::new(format!("cannot load package: {error}")))?;
    let bytes = export_obj(&loaded.mesh, &loaded.materials)
        .map_err(|error| CliError::new(format!("cannot export OBJ: {error}")))?;
    write_bytes_atomic(output, &bytes)?;
    eprintln!(
        "fightbox: city OBJ written to {} (triangles={})",
        output.display(),
        loaded.mesh.triangles.len()
    );
    Ok(())
}

pub fn metamorphic(geojson: &Path, output: &Path) -> Result<()> {
    require_linked("city metamorphic")?;
    let input = std::fs::read(geojson)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", geojson.display())))?;
    let normalized = normalize_geojson_coordinates(&input)?;
    let root: Value = serde_json::from_slice(&normalized)
        .map_err(|error| CliError::new(format!("invalid normalized GeoJSON: {error}")))?;
    let base_provider = city_provider(&normalized);
    let base_geometry = base_provider
        .provide()
        .map_err(|error| CliError::new(format!("cannot inspect GeoJSON assumptions: {error}")))?;
    if base_geometry.assumptions.is_empty() {
        return Err(CliError::new(
            "city metamorphic requires at least one assumption-row building",
        ));
    }
    if !base_geometry
        .assumptions
        .iter()
        .any(|row| row.building_id == OCCLUDER_ID)
    {
        return Err(CliError::new(format!(
            "city metamorphic fixture requires assumption-row building {OCCLUDER_ID}"
        )));
    }
    let target_footprint = feature_footprint(&root, OCCLUDER_ID)?;
    let output = validate_output_path(output)?;
    let directory = AtomicDir::create(output.clone())?;
    let stage = directory.temp_path();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/city/metamorphic/fixture.json");
    let materials = MaterialTable::default();
    let provenance = [Provenance::from_bytes(geojson.to_string_lossy(), &input)];
    let mut assumptions = base_geometry.assumptions.clone();
    assumptions.sort_by(|left, right| left.building_id.cmp(&right.building_id));
    let mut building_reports = Vec::new();

    for assumption in &assumptions {
        let heights = jitter_heights(assumption.assumed_height_m);
        let building_dir = safe_component(&assumption.building_id);
        let mut variants = Vec::new();
        for (label, height) in ["minus", "baseline", "plus"].into_iter().zip(heights) {
            let mut variant_root = root.clone();
            set_feature_height(&mut variant_root, &assumption.building_id, height)?;
            let variant_bytes = serde_json::to_vec(&variant_root).map_err(|error| {
                CliError::new(format!("cannot encode GeoJSON variant: {error}"))
            })?;
            let provider = city_provider(&variant_bytes);
            let geometry = provider
                .provide()
                .map_err(|error| CliError::new(format!("cannot compile variant: {error}")))?;
            let mesh = compile(&provider, &materials, CompileOptions::default())
                .map_err(|error| CliError::new(format!("cannot compile variant mesh: {error}")))?;
            let variant_assumptions = assumptions
                .iter()
                .map(|row| {
                    if row.building_id == assumption.building_id {
                        Assumption {
                            building_id: row.building_id.clone(),
                            assumed_height_m: height,
                            reason: format!(
                                "{}; metamorphic {} variant",
                                row.reason, assumption.building_id
                            ),
                        }
                    } else {
                        row.clone()
                    }
                })
                .collect();
            let package_relative = PathBuf::from("variants")
                .join(&building_dir)
                .join(format!("{label}.fightbox"));
            let package = stage.join(&package_relative);
            write_package_with_metadata(
                &package,
                &mesh,
                &materials,
                &provenance,
                env!("CARGO_PKG_VERSION"),
                &PackageMetadata {
                    building_count: geometry.building_count,
                    assumptions: variant_assumptions,
                },
            )
            .map_err(|error| CliError::new(format!("cannot write variant package: {error}")))?;
            let proof = prove_percept_geometry(&mesh, &target_footprint)?;
            let baked_relative = PathBuf::from("bakes")
                .join(&building_dir)
                .join(format!("{label}.baked"));
            let baked_path = stage.join(&baked_relative);
            std::fs::create_dir_all(
                baked_path
                    .parent()
                    .expect("metamorphic bake path has a parent"),
            )
            .map_err(|error| {
                CliError::new(format!(
                    "cannot create metamorphic bake parent {}: {error}",
                    baked_path.parent().unwrap().display()
                ))
            })?;
            bake(&package, &baked_path)?;
            let loaded = read_package(&package).map_err(|error| {
                CliError::new(format!("cannot reload variant package: {error}"))
            })?;
            let baked = load_baked(&baked_path)?;
            let probe_batch_sha256 = baked.metadata.content_sha256.clone();
            let levels = crate::phase_b::measure_city_occlusion_percept(
                &fixture,
                scene_mesh(&loaded)?,
                &baked,
            )?;
            variants.push(MetamorphicVariantReport {
                label,
                height_m: height,
                package: slash_path(&package_relative),
                probe_batch_sha256,
                geometry: proof,
                shadow_rms_dbfs: levels.shadow_rms_dbfs,
                los_rms_dbfs: levels.los_rms_dbfs,
                los_minus_shadow_db: levels.los_rms_dbfs - levels.shadow_rms_dbfs,
                passed: false,
            });
        }
        let baseline_delta = variants[1].los_minus_shadow_db;
        let margin_db = (baseline_delta / 2.0).floor().max(3.0);
        for variant in &mut variants {
            variant.passed = variant.los_minus_shadow_db >= margin_db;
        }
        building_reports.push(MetamorphicBuildingReport {
            building_id: assumption.building_id.clone(),
            base_assumed_height_m: assumption.assumed_height_m,
            margin_db,
            margin_derivation: "max(3.0 dB, floor(measured baseline delta / 2))",
            passed: variants.iter().all(|variant| variant.passed),
            variants,
        });
    }

    std::fs::remove_dir_all(stage.join("bakes")).map_err(|error| {
        CliError::new(format!(
            "cannot remove transient metamorphic bakes after measurement: {error}"
        ))
    })?;
    let passed = building_reports.iter().all(|building| building.passed);
    let report = MetamorphicReport {
        schema_version: "fightbox.city-metamorphic.v1",
        geojson: geojson.to_string_lossy().into_owned(),
        jitter_m: JITTER_M,
        minimum_height_m: MIN_ASSUMED_HEIGHT_M,
        fixture: MetamorphicFixtureReport {
            path: "fixtures/city/metamorphic/fixture.json".to_owned(),
            source_m: PERCEPT_SOURCE_M,
            shadow_listener_m: PERCEPT_SHADOW_LISTENER_M,
            los_listener_m: PERCEPT_LOS_LISTENER_M,
            occluder_building_id: OCCLUDER_ID,
        },
        buildings: building_reports,
        passed,
    };
    write_json_atomic(&stage.join("report.json"), &report)?;
    directory.commit()?;
    if !passed {
        return Err(CliError::new(format!(
            "city metamorphic percept failed; report written to {}",
            output.join("report.json").display()
        )));
    }
    eprintln!(
        "fightbox: city metamorphic report written to {} (passed)",
        output.display()
    );
    Ok(())
}

fn inspect_text(package: &Path) -> Result<String> {
    let loaded = read_package(package)
        .map_err(|error| CliError::new(format!("cannot inspect package: {error}")))?;
    let manifest = &loaded.manifest;
    let mut output = format!(
        "format version: {}\nbuildings: {}\ntriangles: {}\nmaterial bands: low, mid, high\nmesh sha256: {}\nmaterials sha256: {}\n",
        manifest.format_version,
        manifest.building_count,
        manifest.triangle_count,
        manifest.mesh_content_sha256,
        manifest.materials_content_sha256,
    );
    if let Some(schema_version) = &manifest.schema_version {
        output.push_str(&format!("schema version: {schema_version}\n"));
    }
    if let Some(world) = &manifest.world {
        let origin = &world.city.geodetic_origin;
        output.push_str(&format!(
            "city: {}\ngeodetic origin: {} frame={} latitude={} longitude={} altitude_m={}\ncell: {} grid=e{},n{} local_to_city_enu_m={:?}\ncell bounds local ENU m: min={:?} max={:?}\ncell policy: probe_footprint_m={} stride_m={} overlap_m={} ownership_guard_m={} geometry_halo_m={} baked_path_horizon_m={} resident_worlds={} prepared_neighbors={} target_raw_probe_bytes={} hard_raw_probe_bytes={}\n",
            world.city.id,
            origin.datum,
            origin.local_frame,
            origin.latitude_degrees,
            origin.longitude_degrees,
            origin.altitude_m,
            world.cell.id,
            world.cell.grid_index.east,
            world.cell.grid_index.north,
            world.cell.local_to_city_enu_m,
            world.cell.bounds_local_enu_m.min_enu_m,
            world.cell.bounds_local_enu_m.max_enu_m,
            world.cell.supported_ranges.probe_footprint_m,
            world.mobile_cell_policy.stride_m,
            world.mobile_cell_policy.pairwise_overlap_m,
            world.mobile_cell_policy.ownership_guard_m,
            world.cell.supported_ranges.geometry_halo_m,
            world.cell.supported_ranges.baked_path_horizon_m,
            world.mobile_cell_policy.maximum_resident_worlds,
            world.mobile_cell_policy.maximum_prepared_neighbors,
            world.mobile_cell_policy.target_raw_probe_payload_bytes,
            world.mobile_cell_policy.hard_raw_probe_payload_bytes,
        ));
        output.push_str("extensions:\n");
        if manifest.extensions.is_empty() {
            output.push_str("  (none)\n");
        } else {
            for extension in &manifest.extensions {
                output.push_str(&format!(
                    "  {} {:?} path={} raw_sha256={} stored_sha256={} raw_bytes={} stored_bytes={} compression={:?}\n",
                    extension.capability,
                    extension.requirement,
                    extension.path,
                    extension.content_sha256,
                    extension.stored_content_sha256,
                    extension.raw_size_bytes,
                    extension.stored_size_bytes,
                    extension.compression,
                ));
            }
        }
        output.push_str("unsupported optional capabilities:\n");
        if loaded.unsupported_optional_capabilities.is_empty() {
            output.push_str("  (none)\n");
        } else {
            for capability in &loaded.unsupported_optional_capabilities {
                output.push_str(&format!("  {capability}\n"));
            }
        }
    }
    output.push_str("materials:\n");
    for (name, material) in manifest.materials.iter() {
        output.push_str(&format!(
            "  {name}: absorption={:?} scattering={} transmission={:?}\n",
            material.absorption, material.scattering, material.transmission
        ));
    }
    output.push_str("inputs:\n");
    for input in &manifest.inputs {
        output.push_str(&format!("  {} sha256={}\n", input.path, input.sha256));
    }
    output.push_str("assumptions:\n");
    if manifest.assumptions.is_empty() {
        output.push_str("  (none)\n");
    } else {
        for row in &manifest.assumptions {
            output.push_str(&format!(
                "  {} -> {} m ({})\n",
                row.building_id, row.assumed_height_m, row.reason
            ));
        }
    }
    Ok(output)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BakeConfig {
    pub path_range_m: f32,
    pub visibility_range_m: f32,
    pub visibility_samples: i32,
    pub visibility_threshold: f32,
    pub probe_spacing_m: f32,
    pub probe_height_above_floor_m: f32,
    pub probe_ceiling_m: f32,
    /// Absolute ENU altitudes, in metres, of extra mid-air probe layers. Empty
    /// by default; each entry adds one flat layer over the same horizontal
    /// footprint as the floor probes.
    pub elevated_probe_layers_m: Vec<f32>,
    /// Horizontal spacing, in metres, shared by every elevated layer. `None`
    /// follows [`Self::probe_spacing_m`].
    ///
    /// Elevated layers rarely want the floor's density: a layer above the
    /// rooves exists so an airborne source has *some* influencing probe, and
    /// each of those probes sees far more neighbours than a street-level one,
    /// so halving the density quarters the layer's contribution to the baked
    /// path data.
    pub elevated_probe_spacing_m: Option<f32>,
    pub bake_threads: i32,
}

impl Default for BakeConfig {
    fn default() -> Self {
        let pathing = PathBakeConfig::default();
        Self {
            path_range_m: pathing.path_range_m,
            visibility_range_m: pathing.visibility_range_m,
            visibility_samples: pathing.num_visibility_samples,
            visibility_threshold: pathing.visibility_threshold,
            probe_spacing_m: PROBE_SPACING_M,
            probe_height_above_floor_m: PROBE_HEIGHT_M,
            probe_ceiling_m: PROBE_CEILING_M,
            elevated_probe_layers_m: Vec::new(),
            elevated_probe_spacing_m: None,
            bake_threads: pathing.num_threads,
        }
    }
}

pub(crate) fn city_bake_manifest(
    materials_content_sha256: &str,
    mesh_content_sha256: &str,
    probe_batch_sha256: &str,
    config: &BakeConfig,
    bake_duration_s: f64,
) -> Value {
    let elevated_spacing_m = config
        .elevated_probe_spacing_m
        .unwrap_or(config.probe_spacing_m);
    let elevated_probe_layers = config
        .elevated_probe_layers_m
        .iter()
        .map(|height_enu_m| {
            json!({
                "height_enu_m": height_enu_m,
                "spacing_m": elevated_spacing_m,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema_version": "fightbox.city-bake.v1",
        "materials_content_sha256": materials_content_sha256,
        "mesh_content_sha256": mesh_content_sha256,
        "probe_batch_sha256": probe_batch_sha256,
        "bake_config": {
            "probe_ceiling_m": config.probe_ceiling_m,
            "floor_probe_spacing_m": config.probe_spacing_m,
            "probe_height_above_floor_m": config.probe_height_above_floor_m,
            "elevated_probe_layers": elevated_probe_layers,
            "path_range_m": config.path_range_m,
            "visibility_range_m": config.visibility_range_m,
            "visibility_samples": config.visibility_samples,
            "visibility_threshold": config.visibility_threshold,
            "threads": config.bake_threads,
        },
        "bake_duration_s": bake_duration_s,
    })
}

/// The mid-air layers `config` asks for, at their effective spacing.
///
/// Probe positions are a pure function of the layer, the probe volume, and the
/// mesh, so a config that leaves `elevated_probe_spacing_m` unset produces the
/// exact layers — and therefore the exact probe positions — it produced before
/// the spacing was separable at all.
fn elevated_probe_layers(config: &BakeConfig) -> Vec<ElevatedProbeLayer> {
    let spacing_m = config
        .elevated_probe_spacing_m
        .unwrap_or(config.probe_spacing_m);
    config
        .elevated_probe_layers_m
        .iter()
        .map(|height_enu_m| ElevatedProbeLayer {
            height_enu_m: *height_enu_m,
            spacing_m,
        })
        .collect()
}

/// Conservative disk budget derived from the probe layout and path radius.
///
/// Steam Audio's uniform-floor layout has at most `floor(span / spacing) + 1`
/// probes on either horizontal axis. Elevated layers use that same grid and
/// may only remove probes buried in solids, so summing the unculled grids is a
/// safe probe-count bound before the SDK starts its expensive path bake.
///
/// Retained Steam Audio 4.8.1 city serializations use fewer than six bytes per
/// potentially reachable ordered probe pair. Ten bytes leaves headroom for
/// route/schema variation, 256 bytes per probe covers batch bookkeeping, and a
/// fixed 64 KiB covers serialization roots plus the two JSON sidecars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BakeArtifactEstimate {
    pub(crate) probe_count_upper_bound: u64,
    probe_batch_bytes: u64,
    pub(crate) bytes: u64,
    /// Probe pairs within the path range, which drive the bake's peak memory.
    pub(crate) probe_pairs: u64,
}

/// Identity of a non-empty mask for the bake key and manifest. Routes enter by
/// hash so a long GPX trace does not bloat every manifest.
pub(crate) fn probe_mask_identity(mask: &ProbeMask) -> Value {
    let mut identity = json!({
        "drop_over_solids": mask.drop_over_solids,
        "corridor": mask.corridor.as_ref().map(|corridor| json!({
            "half_width_m": corridor.half_width_m,
            "route_count": corridor.routes_enu_m.len(),
            "routes_sha256": fightbox_evidence::sha256_hex(
                &serde_json::to_vec(&corridor.routes_enu_m).unwrap_or_default()),
            "islands_enu_m": corridor.islands_enu_m,
        })),
    });
    // Present only when used, so every pre-graded mask keeps its bake key.
    if let Some(graded) = &mask.graded {
        identity["graded"] = json!({
            "coarse_stride": graded.coarse_stride,
            "fine_half_width_m": graded.fine.half_width_m,
            "fine_route_count": graded.fine.routes_enu_m.len(),
            "fine_island_count": graded.fine.islands_enu_m.len(),
            "fine_sha256": fightbox_evidence::sha256_hex(
                &serde_json::to_vec(&(&graded.fine.routes_enu_m, &graded.fine.islands_enu_m))
                    .unwrap_or_default()),
        });
    }
    identity
}

fn estimate_masked_bake_artifact(
    probes: ProbeVolume,
    mesh: &SceneMesh,
    mask: &ProbeMask,
    config: &BakeConfig,
) -> Result<BakeArtifactEstimate> {
    let layers = elevated_probe_layers(config);
    if mask.is_empty() {
        return estimate_bake_artifact(probes, &layers, config.path_range_m);
    }
    let floor = fightbox_steam_audio::masked_floor_probe_count(probes, mesh, mask).max(1);
    estimate_counted_bake_artifact(floor, probes, &layers, config.path_range_m)
}

fn estimate_bake_artifact(
    probes: ProbeVolume,
    elevated_layers: &[ElevatedProbeLayer],
    path_range_m: f32,
) -> Result<BakeArtifactEstimate> {
    estimate_counted_bake_artifact(
        grid_probe_count(probes, probes.spacing_m)?,
        probes,
        elevated_layers,
        path_range_m,
    )
}

fn estimate_counted_bake_artifact(
    floor_probe_count: u64,
    probes: ProbeVolume,
    elevated_layers: &[ElevatedProbeLayer],
    path_range_m: f32,
) -> Result<BakeArtifactEstimate> {
    let mut probe_count = floor_probe_count;
    let mut minimum_spacing_m = probes.spacing_m;
    for layer in elevated_layers {
        probe_count = probe_count
            .checked_add(grid_probe_count(probes, layer.spacing_m)?)
            .ok_or_else(|| CliError::new("city probe layout count overflowed u64"))?;
        minimum_spacing_m = minimum_spacing_m.min(layer.spacing_m);
    }

    let layer_count = u64::try_from(elevated_layers.len())
        .ok()
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| CliError::new("city probe layer count overflowed u64"))?;
    let radius_in_cells = f64::from(path_range_m) / f64::from(minimum_spacing_m);
    // Every lattice point owns a unit square. If its centre is within the path
    // radius, that square lies within a circle expanded by half the square's
    // diagonal; the expanded circle's area therefore bounds the point count.
    let expanded_radius = radius_in_cells + std::f64::consts::FRAC_1_SQRT_2;
    let reachable_per_layer = f64_to_u64_ceil(
        std::f64::consts::PI * expanded_radius * expanded_radius,
        "city path-neighbour estimate exceeds u64",
    )?;
    let reachable_per_probe = reachable_per_layer
        .checked_mul(layer_count)
        .unwrap_or(u64::MAX)
        .min(probe_count);

    let per_probe = u128::from(SERIALIZED_BYTES_PER_PROBE)
        + u128::from(SERIALIZED_BYTES_PER_REACHABLE_PAIR) * u128::from(reachable_per_probe);
    let probe_batch_bytes = u64::try_from(u128::from(probe_count) * per_probe).map_err(|_| {
        CliError::new("estimated city bake artifact exceeds the supported u64 file size")
    })?;
    let bytes = BAKE_ARTIFACT_FIXED_BYTES
        .checked_add(probe_batch_bytes)
        .ok_or_else(|| {
            CliError::new("estimated city bake artifact exceeds the supported u64 file size")
        })?;
    let probe_pairs = u64::try_from(
        u128::from(probe_count) * u128::from(reachable_per_probe.saturating_sub(1)) / 2,
    )
    .unwrap_or(u64::MAX);
    Ok(BakeArtifactEstimate {
        probe_count_upper_bound: probe_count,
        probe_batch_bytes,
        bytes,
        probe_pairs,
    })
}

fn grid_probe_count(probes: ProbeVolume, spacing_m: f32) -> Result<u64> {
    let east = grid_axis_count(probes.min_enu_m.x, probes.max_enu_m.x, spacing_m)?;
    let north = grid_axis_count(probes.min_enu_m.y, probes.max_enu_m.y, spacing_m)?;
    east.checked_mul(north)
        .ok_or_else(|| CliError::new("city probe layout count overflowed u64"))
}

fn grid_axis_count(min: f32, max: f32, spacing_m: f32) -> Result<u64> {
    let span = f64::from(max) - f64::from(min);
    if !span.is_finite() || span < 0.0 || !spacing_m.is_finite() || spacing_m <= 0.0 {
        return Err(CliError::new(
            "city probe layout requires finite bounds and positive spacing",
        ));
    }
    let steps = (span / f64::from(spacing_m)).floor();
    f64_to_u64_ceil(steps + 1.0, "city probe axis count exceeds u64")
}

fn f64_to_u64_ceil(value: f64, overflow_message: &'static str) -> Result<u64> {
    if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 {
        return Err(CliError::new(overflow_message));
    }
    Ok(value.ceil() as u64)
}

pub(crate) fn package_hash(package: &Path) -> Result<String> {
    let mut hashes = Vec::new();
    for name in ["manifest.json", "materials.json", "mesh.bin"] {
        let bytes = std::fs::read(package.join(name))
            .map_err(|error| CliError::new(format!("cannot hash package {name}: {error}")))?;
        hashes.extend_from_slice(fightbox_evidence::sha256_hex(&bytes).as_bytes());
    }
    Ok(fightbox_evidence::sha256_hex(&hashes))
}

pub(crate) fn bake_estimate(package: &Path, config: &BakeConfig) -> Result<(u64, f64, u64)> {
    let (estimate, area_m2) = bake_estimate_masked(package, config, &ProbeMask::default())?;
    Ok((estimate.probe_count_upper_bound, area_m2, estimate.bytes))
}

/// Like [`bake_estimate`], counting only the floor probes `mask` keeps on the
/// emulated uniform-floor grid, so the preflight bound follows the mask.
pub(crate) fn bake_estimate_masked(
    package: &Path,
    config: &BakeConfig,
    mask: &ProbeMask,
) -> Result<(BakeArtifactEstimate, f64)> {
    let loaded = read_package(package)
        .map_err(|error| CliError::new(format!("cannot load package: {error}")))?;
    let mesh = scene_mesh(&loaded)?;
    let probes = probe_volume(&mesh, config)?;
    let estimate = estimate_masked_bake_artifact(probes, &mesh, mask, config)?;
    let area_m2 = f64::from(probes.max_enu_m.x - probes.min_enu_m.x)
        * f64::from(probes.max_enu_m.y - probes.min_enu_m.y);
    Ok((estimate, area_m2))
}

pub fn bake(package: &Path, output: &Path) -> Result<()> {
    bake_with_config(package, output, BakeConfig::default())
}

pub(crate) fn bake_with_config(package: &Path, output: &Path, config: BakeConfig) -> Result<()> {
    bake_with_progress(package, output, config, None)
}

pub(crate) fn bake_with_progress(
    package: &Path,
    output: &Path,
    config: BakeConfig,
    progress: Option<&(dyn Fn(f32) + Send + Sync)>,
) -> Result<()> {
    bake_masked(
        package,
        output,
        config,
        &ProbeMask::default(),
        progress,
        &BakeGuard::host_default(),
    )
}

/// Bakes with `mask` dropping generated floor probes; the default mask is the
/// legacy bake, byte for byte. `guard` refuses a bake whose estimated peak
/// memory passes its cap and cancels one whose real footprint does.
pub(crate) fn bake_masked(
    package: &Path,
    output: &Path,
    config: BakeConfig,
    mask: &ProbeMask,
    progress: Option<&(dyn Fn(f32) + Send + Sync)>,
    guard: &BakeGuard,
) -> Result<()> {
    require_linked("city bake")?;
    let loaded = read_package(package)
        .map_err(|error| CliError::new(format!("cannot load package: {error}")))?;
    let scene = scene_mesh(&loaded)?;
    let probes = probe_volume(&scene, &config)?;
    let elevated_layers = elevated_probe_layers(&config);
    let estimate = estimate_masked_bake_artifact(probes, &scene, mask, &config)?;
    guard.budget.check(estimate.probe_pairs)?;
    // Claim the destination before the bake, not after. A city-scale path bake
    // is minutes of work; discovering an unwritable, occupied, or full output
    // at the end of it throws all of that away. The large file itself is filled
    // with zeroes here so APFS must allocate its blocks before compute starts.
    let output = validate_output_path(output)?;
    let directory = AtomicDir::create(output.clone())?;
    let temp = directory.temp_path();
    let reservation =
        BakeReservation::create(&temp.join("probe-batch.bin"), &output, estimate.bytes)?;
    eprintln!(
        "fightbox: city bake preflight reserved {} for at most {} probes (filesystem reported {} available; APFS purgeable space can make that optimistic)",
        format_bytes(estimate.bytes),
        estimate.probe_count_upper_bound,
        format_bytes(reservation.reported_available_bytes())
    );

    let defaults = PathBakeConfig::default();
    let bake_started = Instant::now();
    let request = S3BakeRequest {
        mesh: scene,
        probes,
        elevated_probe_layers: elevated_layers,
        pathing: PathBakeConfig {
            num_visibility_samples: config.visibility_samples,
            probe_visibility_radius_m: defaults.probe_visibility_radius_m,
            visibility_threshold: config.visibility_threshold,
            visibility_range_m: config.visibility_range_m,
            path_range_m: config.path_range_m,
            num_threads: config.bake_threads,
        },
    };
    let mut staging_roots = guard.staging_roots.clone();
    staging_roots.extend(output.parent().map(Path::to_path_buf));
    let watchdog = Watchdog::new(
        guard.budget.cap_bytes().unwrap_or(u64::MAX),
        staging_roots,
        guard.lock.clone(),
    );
    forbid_crash_dumps();
    let should_cancel = || watchdog.should_cancel();
    let baked = fightbox_steam_audio::bake_s3_masked(&request, mask, progress, Some(&should_cancel))
        .map_err(|error| CliError::new(format!("city bake failed: {error}")))?;
    let bake_duration_s = bake_started.elapsed().as_secs_f64();
    eprintln!(
        "fightbox: bake peak footprint {} (estimate {})",
        format_bytes(watchdog.peak_bytes()),
        format_bytes(crate::bake_memory::peak_estimate_bytes(estimate.probe_pairs))
    );
    baked
        .validate()
        .map_err(|error| CliError::new(format!("city bake validation failed: {error}")))?;

    let actual_probe_batch_bytes = u64::try_from(baked.bytes.len())
        .map_err(|_| CliError::new("city probe batch exceeds the supported file size"))?;
    if actual_probe_batch_bytes > estimate.probe_batch_bytes {
        return Err(CliError::new(format!(
            "city probe batch is {} but its conservative pre-compute budget was {}; the artifact estimator was exceeded",
            format_bytes(actual_probe_batch_bytes),
            format_bytes(estimate.probe_batch_bytes)
        )));
    }
    let metadata_json = baked.metadata.to_json();
    let mut city_manifest = city_bake_manifest(
        &loaded.manifest.materials_content_sha256,
        &loaded.manifest.mesh_content_sha256,
        &baked.metadata.content_sha256,
        &config,
        bake_duration_s,
    );
    city_manifest["package_content_sha256"] = json!(package_hash(package)?);
    if !mask.is_empty() {
        city_manifest["probe_mask"] = probe_mask_identity(mask);
    }
    let city_manifest_json = city_manifest.to_string();
    let additional_artifact_bytes =
        u64::try_from(metadata_json.len().saturating_add(city_manifest_json.len()))
            .map_err(|_| CliError::new("city bake sidecar byte count exceeds u64"))?;
    reservation.assert_post_bake(
        actual_probe_batch_bytes,
        baked.metadata.serialized_size_bytes,
        additional_artifact_bytes,
    )?;
    reservation.finish(&baked.bytes)?;
    write_json_string_atomic(&temp.join("probe-batch-metadata.json"), &metadata_json)?;
    write_json_string_atomic(&temp.join("city-bake-manifest.json"), &city_manifest_json)?;
    directory.commit()?;
    eprintln!(
        "fightbox: city bake written to {} (probes={}, elevated_layers_m={:?}, \
         elevated_spacing_m={}, sha256={})",
        output.display(),
        baked.metadata.probe_count,
        config.elevated_probe_layers_m,
        config
            .elevated_probe_spacing_m
            .unwrap_or(config.probe_spacing_m),
        baked.metadata.content_sha256
    );
    Ok(())
}

pub fn render(package: &Path, baked_path: &Path, fixture: &Path, output: &Path) -> Result<()> {
    require_linked("city render")?;
    // City v2 packages carry a required typed bake-plan extension. Negotiate it
    // at the host-consumer boundary rather than treating a valid route package
    // as an unknown generic package.
    let loaded = read_package_with_capabilities(package, &[CITY_BAKE_V2_CAPABILITY])
        .map_err(|error| CliError::new(format!("cannot load package: {error}")))?;
    let baked = load_baked(baked_path)?;
    verify_bake_identity(&loaded, baked_path, &baked)?;
    let identity = crate::phase_b::CityRenderIdentity {
        mesh_content_sha256: loaded.manifest.mesh_content_sha256.clone(),
        materials_content_sha256: loaded.manifest.materials_content_sha256.clone(),
        probe_batch_sha256: baked.metadata.content_sha256.clone(),
    };
    crate::phase_b::run_city_render(fixture, output, scene_mesh(&loaded)?, &baked, identity)
}

pub(crate) fn scene_mesh(loaded: &fightbox_world::LoadedPackage) -> Result<SceneMesh> {
    let mut triangles = Vec::with_capacity(loaded.mesh.triangles.len());
    for triangle in &loaded.mesh.triangles {
        triangles.push([
            i32::try_from(triangle[0])
                .map_err(|_| CliError::new("city mesh index exceeds Steam Audio i32 range"))?,
            i32::try_from(triangle[1])
                .map_err(|_| CliError::new("city mesh index exceeds Steam Audio i32 range"))?,
            i32::try_from(triangle[2])
                .map_err(|_| CliError::new("city mesh index exceeds Steam Audio i32 range"))?,
        ]);
    }
    let material_indices = loaded
        .mesh
        .material_ids
        .iter()
        .map(|index| {
            i32::try_from(*index)
                .map_err(|_| CliError::new("city material index exceeds Steam Audio i32 range"))
        })
        .collect::<Result<Vec<_>>>()?;
    let materials = loaded
        .materials
        .iter()
        .map(|(_, material)| AcousticMaterial {
            absorption: material.absorption,
            scattering: material.scattering,
            transmission: material.transmission,
        })
        .collect();
    Ok(SceneMesh {
        vertices_enu_m: loaded
            .mesh
            .vertices_enu_m
            .iter()
            .map(|vertex| {
                fightbox_steam_audio::EnuVector3::new(vertex.east_m, vertex.north_m, vertex.up_m)
            })
            .collect(),
        triangles,
        material_indices,
        materials,
    })
}

pub(crate) fn probe_volume(mesh: &SceneMesh, config: &BakeConfig) -> Result<ProbeVolume> {
    let first = *mesh
        .vertices_enu_m
        .first()
        .ok_or_else(|| CliError::new("city mesh has no vertices"))?;
    let mut min = first;
    let mut max = first;
    for vertex in &mesh.vertices_enu_m[1..] {
        min.x = min.x.min(vertex.x);
        min.y = min.y.min(vertex.y);
        max.x = max.x.max(vertex.x);
        max.y = max.y.max(vertex.y);
    }
    min.z = 0.0;
    max.z = config.probe_ceiling_m;
    Ok(ProbeVolume {
        min_enu_m: min,
        max_enu_m: max,
        spacing_m: config.probe_spacing_m,
        height_above_floor_m: config.probe_height_above_floor_m,
    })
}

pub(crate) fn load_baked(path: &Path) -> Result<BakedProbeBatch> {
    let bytes = std::fs::read(path.join("probe-batch.bin"))
        .map_err(|error| CliError::new(format!("cannot read probe batch: {error}")))?;
    let metadata_text = std::fs::read_to_string(path.join("probe-batch-metadata.json"))
        .map_err(|error| CliError::new(format!("cannot read probe metadata: {error}")))?;
    let wire: ProbeMetadataWire = serde_json::from_str(&metadata_text)
        .map_err(|error| CliError::new(format!("invalid probe metadata: {error}")))?;
    if wire.schema_version != PROBE_BATCH_METADATA_SCHEMA
        || wire.steam_audio_version != STEAM_AUDIO_VERSION
        || wire.upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT
    {
        return Err(CliError::new(
            "probe metadata does not match the linked Steam Audio backend",
        ));
    }
    let baked = BakedProbeBatch {
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: wire.probe_count,
            path_data_size_bytes: wire.path_data_size_bytes,
            serialized_size_bytes: wire.serialized_size_bytes,
            content_sha256: wire.content_sha256,
            bake_progress_callback_count: wire.bake_progress_callback_count,
            final_bake_progress_millionths: wire.final_bake_progress_millionths,
        },
        bytes,
    };
    baked
        .validate()
        .map_err(|error| CliError::new(format!("invalid baked probe batch: {error}")))?;
    Ok(baked)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeMetadataWire {
    schema_version: String,
    steam_audio_version: String,
    upstream_commit: String,
    probe_count: u32,
    path_data_size_bytes: u64,
    serialized_size_bytes: u64,
    content_sha256: String,
    bake_progress_callback_count: u32,
    final_bake_progress_millionths: u32,
}

pub(crate) fn verify_bake_identity(
    loaded: &fightbox_world::LoadedPackage,
    baked_path: &Path,
    baked: &BakedProbeBatch,
) -> Result<()> {
    let bytes = std::fs::read(baked_path.join("city-bake-manifest.json"))
        .map_err(|error| CliError::new(format!("cannot read city bake manifest: {error}")))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::new(format!("invalid city bake manifest: {error}")))?;
    for (field, expected) in [
        (
            "mesh_content_sha256",
            loaded.manifest.mesh_content_sha256.as_str(),
        ),
        (
            "materials_content_sha256",
            loaded.manifest.materials_content_sha256.as_str(),
        ),
        ("probe_batch_sha256", baked.metadata.content_sha256.as_str()),
    ] {
        if value.get(field).and_then(Value::as_str) != Some(expected) {
            return Err(CliError::new(format!(
                "bake was produced from a different package ({field} mismatch)"
            )));
        }
    }
    Ok(())
}

pub(crate) fn require_linked(command: &str) -> Result<()> {
    if fightbox_steam_audio::backend_availability()
        .to_json()
        .contains(r#""status":"available""#)
    {
        Ok(())
    } else {
        Err(CliError::new(format!(
            "{command} requires --features linked-sdk and STEAM_AUDIO_SDK_DIR"
        )))
    }
}

fn prepare_package_output(output: &Path) -> Result<()> {
    if output.exists()
        && output
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true)
    {
        return Err(CliError::new(format!(
            "package output {} already exists and is non-empty",
            output.display()
        )));
    }
    Ok(())
}

/// OSM fixtures use WGS84 longitude/latitude while the world provider consumes
/// local ENU metres. Synthetic already-local fixtures pass through unchanged.
fn normalize_geojson_coordinates(input: &[u8]) -> Result<Vec<u8>> {
    normalize_geojson_coordinates_with_projection(input, None, false)
}

fn normalize_geojson_coordinates_at_origin(
    input: &[u8],
    origin: &GeodeticOrigin,
    grid_index: CellGridIndex,
) -> Result<Vec<u8>> {
    normalize_geojson_coordinates_with_projection(
        input,
        Some(CoordinateProjection::MobileCell { origin, grid_index }),
        true,
    )
}

fn normalize_geojson_coordinates_for_oracle(
    input: &[u8],
    origin: &GeodeticOrigin,
    geometry_bounds_city_enu_mm: CityBoundsMm,
) -> Result<Vec<u8>> {
    if geometry_bounds_city_enu_mm.min[0] >= geometry_bounds_city_enu_mm.max[0]
        || geometry_bounds_city_enu_mm.min[1] >= geometry_bounds_city_enu_mm.max[1]
    {
        return Err(CliError::new(
            "oracle geometry bounds must increase on both horizontal axes",
        ));
    }
    normalize_geojson_coordinates_with_projection(
        input,
        Some(CoordinateProjection::MonolithicOracle {
            origin,
            geometry_bounds_city_enu_mm,
        }),
        true,
    )
}

#[derive(Clone, Copy)]
enum CoordinateProjection<'a> {
    MobileCell {
        origin: &'a GeodeticOrigin,
        grid_index: CellGridIndex,
    },
    MonolithicOracle {
        origin: &'a GeodeticOrigin,
        geometry_bounds_city_enu_mm: CityBoundsMm,
    },
}

fn normalize_geojson_coordinates_with_projection(
    input: &[u8],
    forced_projection: Option<CoordinateProjection<'_>>,
    force_geographic: bool,
) -> Result<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(input)
        .map_err(|error| CliError::new(format!("invalid GeoJSON: {error}")))?;
    let features = root
        .get_mut("features")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| CliError::new("GeoJSON features must be an array"))?;
    let mut positions = Vec::new();
    for feature in features.iter() {
        let rings = feature
            .pointer("/geometry/coordinates")
            .and_then(Value::as_array)
            .ok_or_else(|| CliError::new("GeoJSON polygon coordinates must be arrays"))?;
        for ring in rings {
            for point in ring
                .as_array()
                .ok_or_else(|| CliError::new("GeoJSON rings must be arrays"))?
            {
                let point = point
                    .as_array()
                    .ok_or_else(|| CliError::new("GeoJSON positions must be arrays"))?;
                positions.push((
                    point.first().and_then(Value::as_f64).ok_or_else(|| {
                        CliError::new("GeoJSON longitude/east coordinate must be numeric")
                    })?,
                    point.get(1).and_then(Value::as_f64).ok_or_else(|| {
                        CliError::new("GeoJSON latitude/north coordinate must be numeric")
                    })?,
                ));
            }
        }
    }
    if (force_geographic || forced_projection.is_some())
        && positions.iter().any(|(longitude, latitude)| {
            !(-180.0..180.0).contains(longitude) || !(-90.0..=90.0).contains(latitude)
        })
    {
        return Err(CliError::new(
            "city compile-v2 requires WGS84 longitude/latitude coordinates",
        ));
    }
    let geographic = force_geographic || positions
        .first()
        .is_some_and(|(east, north)| east.abs() > 45.0 && north.abs() <= 90.0);
    if let Some(projection) = forced_projection {
        let (origin, local_translation_m) = match projection {
            CoordinateProjection::MobileCell { origin, grid_index } => (
                origin,
                [
                    f64::from(grid_index.east) * CELL_STRIDE_M,
                    f64::from(grid_index.north) * CELL_STRIDE_M,
                ],
            ),
            CoordinateProjection::MonolithicOracle { origin, .. } => (origin, [0.0, 0.0]),
        };
        let origin_ecef_m = wgs84_ecef_m(
            origin.latitude_degrees,
            origin.longitude_degrees,
            origin.altitude_m,
        );
        let origin_latitude = origin.latitude_degrees.to_radians();
        let origin_longitude = origin.longitude_degrees.to_radians();
        let (sin_latitude, cos_latitude) = origin_latitude.sin_cos();
        let (sin_longitude, cos_longitude) = origin_longitude.sin_cos();
        transform_geojson_positions(features, |longitude, latitude| {
            let point_ecef_m = wgs84_ecef_m(latitude, longitude, origin.altitude_m);
            let delta = [
                point_ecef_m[0] - origin_ecef_m[0],
                point_ecef_m[1] - origin_ecef_m[1],
                point_ecef_m[2] - origin_ecef_m[2],
            ];
            [
                -sin_longitude * delta[0] + cos_longitude * delta[1] - local_translation_m[0],
                -sin_latitude * cos_longitude * delta[0] - sin_latitude * sin_longitude * delta[1]
                    + cos_latitude * delta[2]
                    - local_translation_m[1],
            ]
        });
        match projection {
            CoordinateProjection::MobileCell { .. } => retain_geometry_halo_features(features),
            CoordinateProjection::MonolithicOracle {
                geometry_bounds_city_enu_mm,
                ..
            } => retain_geometry_bounds_features(features, geometry_bounds_city_enu_mm),
        }
    } else if geographic {
        let count = positions.len() as f64;
        let origin_lon = positions.iter().map(|point| point.0).sum::<f64>() / count;
        let origin_lat = positions.iter().map(|point| point.1).sum::<f64>() / count;
        let cos_lat = origin_lat.to_radians().cos();
        transform_geojson_positions(features, |longitude, latitude| {
            [
                (longitude - origin_lon).to_radians() * EARTH_RADIUS_M * cos_lat,
                (latitude - origin_lat).to_radians() * EARTH_RADIUS_M,
            ]
        });
    }
    serde_json::to_vec(&root)
        .map_err(|error| CliError::new(format!("cannot normalize GeoJSON: {error}")))
}

fn transform_geojson_positions(features: &mut [Value], project: impl Fn(f64, f64) -> [f64; 2]) {
    for feature in features {
        let rings = feature
            .pointer_mut("/geometry/coordinates")
            .and_then(Value::as_array_mut)
            .expect("validated above");
        for ring in rings {
            for point in ring.as_array_mut().expect("validated above") {
                let point = point.as_array_mut().expect("validated above");
                let longitude = point[0].as_f64().expect("validated above");
                let latitude = point[1].as_f64().expect("validated above");
                let [east_m, north_m] = project(longitude, latitude);
                point[0] = Value::from(east_m);
                point[1] = Value::from(north_m);
            }
        }
    }
}

/// A mobile cell owns only geometry intersecting its 585 m probe footprint plus
/// the frozen 600 m material/geometry halo. Whole intersecting features are
/// retained so the compiler never manufactures cut façades at the halo edge.
fn retain_geometry_halo_features(features: &mut Vec<Value>) {
    let half_extent_m = f64::from(CELL_PROBE_FOOTPRINT_M) / 2.0 + f64::from(CELL_GEOMETRY_HALO_M);
    retain_geometry_bounds_features(
        features,
        CityBoundsMm::new(
            [
                (-half_extent_m * 1_000.0) as i64,
                (-half_extent_m * 1_000.0) as i64,
            ],
            [
                (half_extent_m * 1_000.0) as i64,
                (half_extent_m * 1_000.0) as i64,
            ],
        ),
    );
}

/// Retains whole features intersecting the declared city-ENU rectangle. Whole
/// polygons, rather than clipped rings, keep façade topology identical between
/// mobile slices and the monolithic oracle.
fn retain_geometry_bounds_features(features: &mut Vec<Value>, bounds_city_enu_mm: CityBoundsMm) {
    let bounds_min_m = bounds_city_enu_mm.min.map(|value| value as f64 / 1_000.0);
    let bounds_max_m = bounds_city_enu_mm.max.map(|value| value as f64 / 1_000.0);
    features.retain(|feature| {
        let rings = feature
            .pointer("/geometry/coordinates")
            .and_then(Value::as_array)
            .expect("validated above");
        let mut min = [f64::INFINITY; 2];
        let mut max = [f64::NEG_INFINITY; 2];
        for ring in rings {
            for point in ring.as_array().expect("validated above") {
                let point = point.as_array().expect("validated above");
                for axis in 0..2 {
                    let value = point[axis].as_f64().expect("validated above");
                    min[axis] = min[axis].min(value);
                    max[axis] = max[axis].max(value);
                }
            }
        }
        (0..2).all(|axis| max[axis] >= bounds_min_m[axis] && min[axis] <= bounds_max_m[axis])
    });
}

fn wgs84_ecef_m(latitude_degrees: f64, longitude_degrees: f64, altitude_m: f64) -> [f64; 3] {
    let latitude = latitude_degrees.to_radians();
    let longitude = longitude_degrees.to_radians();
    let (sin_latitude, cos_latitude) = latitude.sin_cos();
    let (sin_longitude, cos_longitude) = longitude.sin_cos();
    let prime_vertical_radius_m = WGS84_SEMI_MAJOR_AXIS_M
        / (1.0 - WGS84_FIRST_ECCENTRICITY_SQUARED * sin_latitude * sin_latitude).sqrt();
    [
        (prime_vertical_radius_m + altitude_m) * cos_latitude * cos_longitude,
        (prime_vertical_radius_m + altitude_m) * cos_latitude * sin_longitude,
        (prime_vertical_radius_m * (1.0 - WGS84_FIRST_ECCENTRICITY_SQUARED) + altitude_m)
            * sin_latitude,
    ]
}

fn mesh_bounds(mesh: &AcousticMesh) -> Result<CellBounds> {
    let first = mesh
        .vertices_enu_m
        .first()
        .ok_or_else(|| CliError::new("compiled mesh has no vertices"))?;
    let mut min = [
        f64::from(first.east_m),
        f64::from(first.north_m),
        f64::from(first.up_m),
    ];
    let mut max = min;
    for vertex in &mesh.vertices_enu_m[1..] {
        let point = [
            f64::from(vertex.east_m),
            f64::from(vertex.north_m),
            f64::from(vertex.up_m),
        ];
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    let bounds = CellBounds {
        min_enu_m: min,
        max_enu_m: max,
    };
    bounds
        .validate()
        .map_err(|error| CliError::new(format!("invalid compiled mesh bounds: {error}")))?;
    Ok(bounds)
}

fn city_provider(bytes: &[u8]) -> GeoJsonProvider<'_> {
    GeoJsonProvider::new(
        bytes,
        GeoJsonOptions {
            default_height_m: Some(DEFAULT_HEIGHT_M),
            ..GeoJsonOptions::default()
        },
    )
}

fn jitter_heights(base_height_m: f32) -> [f32; 3] {
    [
        (base_height_m - JITTER_M).max(MIN_ASSUMED_HEIGHT_M),
        base_height_m.max(MIN_ASSUMED_HEIGHT_M),
        (base_height_m + JITTER_M).max(MIN_ASSUMED_HEIGHT_M),
    ]
}

fn feature_matches(feature: &Value, building_id: &str) -> bool {
    feature.get("id").and_then(Value::as_str) == Some(building_id)
        || feature.pointer("/properties/id").and_then(Value::as_str) == Some(building_id)
        || feature.pointer("/properties/name").and_then(Value::as_str) == Some(building_id)
}

fn set_feature_height(root: &mut Value, building_id: &str, height_m: f32) -> Result<()> {
    let features = root
        .get_mut("features")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| CliError::new("GeoJSON features must be an array"))?;
    let feature = features
        .iter_mut()
        .find(|feature| feature_matches(feature, building_id))
        .ok_or_else(|| CliError::new(format!("cannot find assumption building {building_id}")))?;
    let properties = feature
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            CliError::new(format!(
                "assumption building {building_id} properties must be an object"
            ))
        })?;
    properties.insert("height".to_owned(), Value::from(height_m));
    Ok(())
}

fn feature_footprint(root: &Value, building_id: &str) -> Result<Vec<[f64; 2]>> {
    let feature = root
        .get("features")
        .and_then(Value::as_array)
        .and_then(|features| {
            features
                .iter()
                .find(|feature| feature_matches(feature, building_id))
        })
        .ok_or_else(|| CliError::new(format!("cannot find occluder {building_id}")))?;
    let ring = feature
        .pointer("/geometry/coordinates/0")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::new(format!("occluder {building_id} has no exterior ring")))?;
    let mut footprint =
        ring.iter()
            .map(|point| {
                let coordinates = point
                    .as_array()
                    .ok_or_else(|| CliError::new("occluder coordinate must be an array"))?;
                Ok([
                    coordinates
                        .first()
                        .and_then(Value::as_f64)
                        .ok_or_else(|| CliError::new("occluder east coordinate must be numeric"))?,
                    coordinates.get(1).and_then(Value::as_f64).ok_or_else(|| {
                        CliError::new("occluder north coordinate must be numeric")
                    })?,
                ])
            })
            .collect::<Result<Vec<_>>>()?;
    if footprint.first() == footprint.last() {
        footprint.pop();
    }
    Ok(footprint)
}

fn prove_percept_geometry(
    mesh: &AcousticMesh,
    target_footprint: &[[f64; 2]],
) -> Result<GeometryProofReport> {
    let source = vec3(PERCEPT_SOURCE_M);
    let shadow = vec3(PERCEPT_SHADOW_LISTENER_M);
    let los = vec3(PERCEPT_LOS_LISTENER_M);
    let shadow_all = segment_intersections(mesh, source, shadow, |_| true);
    let shadow_target = segment_intersections(mesh, source, shadow, |triangle| {
        triangle.iter().all(|vertex| {
            target_footprint.iter().any(|point| {
                (f64::from(vertex.east_m) - point[0]).abs() <= 1.0e-4
                    && (f64::from(vertex.north_m) - point[1]).abs() <= 1.0e-4
            })
        })
    });
    let clear = segment_intersections(mesh, source, los, |_| true);
    if shadow_all.len() < 2
        || shadow_all.len() != shadow_target.len()
        || !same_parameters(&shadow_all, &shadow_target)
        || !clear.is_empty()
    {
        return Err(CliError::new(format!(
            "percept geometry invariant failed: shadow intersections={} target intersections={} clear intersections={}",
            shadow_all.len(),
            shadow_target.len(),
            clear.len()
        )));
    }
    Ok(GeometryProofReport {
        shadow_intersections: shadow_all.len(),
        shadow_target_intersections: shadow_target.len(),
        clear_intersections: clear.len(),
        sole_occluder_proved: true,
    })
}

fn vec3(value: [f64; 3]) -> EnuVector3 {
    EnuVector3::new(value[0] as f32, value[1] as f32, value[2] as f32)
}

fn segment_intersections(
    mesh: &AcousticMesh,
    start: EnuVector3,
    end: EnuVector3,
    include: impl Fn(&[EnuVector3; 3]) -> bool,
) -> Vec<f64> {
    let direction = subtract3(end, start);
    let mut parameters = Vec::new();
    for triangle in &mesh.triangles {
        let vertices = triangle.map(|index| mesh.vertices_enu_m[index as usize]);
        if include(&vertices) {
            if let Some(parameter) = segment_triangle_parameter(start, direction, vertices) {
                parameters.push(parameter);
            }
        }
    }
    parameters.sort_by(f64::total_cmp);
    parameters.dedup_by(|left, right| (*left - *right).abs() <= 1.0e-7);
    parameters
}

fn segment_triangle_parameter(
    start: EnuVector3,
    direction: EnuVector3,
    triangle: [EnuVector3; 3],
) -> Option<f64> {
    let edge1 = subtract3(triangle[1], triangle[0]);
    let edge2 = subtract3(triangle[2], triangle[0]);
    let p = cross3(direction, edge2);
    let determinant = dot3(edge1, p);
    if determinant.abs() <= 1.0e-9 {
        return None;
    }
    let inverse = 1.0 / determinant;
    let translated = subtract3(start, triangle[0]);
    let u = dot3(translated, p) * inverse;
    if !(-1.0e-9..=1.0 + 1.0e-9).contains(&u) {
        return None;
    }
    let q = cross3(translated, edge1);
    let v = dot3(direction, q) * inverse;
    if v < -1.0e-9 || u + v > 1.0 + 1.0e-9 {
        return None;
    }
    let parameter = dot3(edge2, q) * inverse;
    (parameter > 1.0e-9 && parameter < 1.0 - 1.0e-9).then_some(parameter)
}

fn subtract3(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m - right.east_m,
        left.north_m - right.north_m,
        left.up_m - right.up_m,
    )
}

fn cross3(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.north_m * right.up_m - left.up_m * right.north_m,
        left.up_m * right.east_m - left.east_m * right.up_m,
        left.east_m * right.north_m - left.north_m * right.east_m,
    )
}

fn dot3(left: EnuVector3, right: EnuVector3) -> f64 {
    f64::from(left.east_m) * f64::from(right.east_m)
        + f64::from(left.north_m) * f64::from(right.north_m)
        + f64::from(left.up_m) * f64::from(right.up_m)
}

fn same_parameters(left: &[f64], right: &[f64]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| (left - right).abs() <= 1.0e-7)
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Serialize)]
struct MetamorphicReport {
    schema_version: &'static str,
    geojson: String,
    jitter_m: f32,
    minimum_height_m: f32,
    fixture: MetamorphicFixtureReport,
    buildings: Vec<MetamorphicBuildingReport>,
    passed: bool,
}

#[derive(Serialize)]
struct MetamorphicFixtureReport {
    path: String,
    source_m: [f64; 3],
    shadow_listener_m: [f64; 3],
    los_listener_m: [f64; 3],
    occluder_building_id: &'static str,
}

#[derive(Serialize)]
struct MetamorphicBuildingReport {
    building_id: String,
    base_assumed_height_m: f32,
    margin_db: f64,
    margin_derivation: &'static str,
    variants: Vec<MetamorphicVariantReport>,
    passed: bool,
}

#[derive(Serialize)]
struct MetamorphicVariantReport {
    label: &'static str,
    height_m: f32,
    package: String,
    probe_batch_sha256: String,
    geometry: GeometryProofReport,
    shadow_rms_dbfs: f64,
    los_rms_dbfs: f64,
    los_minus_shadow_db: f64,
    passed: bool,
}

#[derive(Serialize)]
struct GeometryProofReport {
    shadow_intersections: usize,
    shadow_target_intersections: usize,
    clear_intersections: usize,
    sole_occluder_proved: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fightbox-city-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[cfg(feature = "linked-sdk")]
    fn bake_temp(label: &str) -> PathBuf {
        let scratch = if cfg!(unix) {
            PathBuf::from("/tmp/lane-bake-robustness")
        } else {
            std::env::temp_dir().join("lane-bake-robustness")
        };
        std::fs::create_dir_all(&scratch).unwrap();
        let path = scratch.join(format!(
            "fightbox-city-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn compile_is_byte_deterministic_and_inspect_discloses_default_height() {
        let root = temp("compile");
        let source = root.join("block.geojson");
        std::fs::write(
            &source,
            br#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"tagless","properties":{},"geometry":{"type":"Polygon","coordinates":[[[0,0],[4,0],[4,3],[0,3],[0,0]]]}}]}"#,
        )
        .unwrap();
        let first = root.join("first.fightbox");
        let second = root.join("second.fightbox");
        compile_geojson(&source, &first).unwrap();
        compile_geojson(&source, &second).unwrap();
        for file in ["manifest.json", "materials.json", "mesh.bin"] {
            assert_eq!(
                std::fs::read(first.join(file)).unwrap(),
                std::fs::read(second.join(file)).unwrap()
            );
        }
        let output = inspect_text(&first).unwrap();
        assert!(output.contains("buildings: 1"));
        assert!(output.contains("tagless -> 4 m"));
        assert!(output.contains("missing height"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_v2_persists_explicit_origin_and_is_byte_deterministic() {
        let root = temp("compile-v2");
        let source = root.join("wgs84-block.geojson");
        std::fs::write(
            &source,
            br#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"building-1","properties":{"height":12},"geometry":{"type":"Polygon","coordinates":[[[-87.623177,41.881832],[-87.623077,41.881832],[-87.623077,41.881932],[-87.623177,41.881932],[-87.623177,41.881832]]]}}]}"#,
        )
        .unwrap();
        let config = CityCompileV2Config {
            city_id: "chicago-loop".to_owned(),
            geodetic_origin: GeodeticOrigin::wgs84(41.881_832, -87.623_177, 181.0),
            cell_grid_index: CellGridIndex { east: 0, north: 0 },
            probe_policy: None,
        };
        let first = root.join("first.fightbox");
        let second = root.join("second.fightbox");
        compile_geojson_v2(&source, &first, config.clone()).unwrap();
        compile_geojson_v2(&source, &second, config).unwrap();
        for file in ["manifest.json", "materials.json", "mesh.bin"] {
            assert_eq!(
                std::fs::read(first.join(file)).unwrap(),
                std::fs::read(second.join(file)).unwrap()
            );
        }
        let loaded = read_package(&first).unwrap();
        assert!(
            loaded
                .mesh
                .vertices_enu_m
                .iter()
                .any(|vertex| vertex.east_m.abs() < 1.0e-4 && vertex.north_m.abs() < 1.0e-4),
            "the persisted origin must map to local ENU zero"
        );
        assert!(
            loaded.mesh.vertices_enu_m.iter().any(|vertex| {
                (8.0..9.0).contains(&vertex.east_m) && vertex.north_m.abs() < 0.01
            })
        );
        let world = loaded.manifest.world.unwrap();
        assert_eq!(world.city.id, "chicago-loop");
        assert_eq!(world.cell.id, "chicago-loop:e0:n0");
        assert_eq!(
            world.city.geodetic_origin,
            GeodeticOrigin::wgs84(41.881_832, -87.623_177, 181.0)
        );
        let output = inspect_text(&first).unwrap();
        assert!(output.contains("format version: 2"));
        assert!(output.contains("schema version: fightbox.world-manifest.v2"));
        assert!(output.contains("cell: chicago-loop:e0:n0"));
        assert!(output.contains("geometry_halo_m=600"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_v2_halo_slices_geometry_and_indexes_the_exact_probe_plan() {
        let root = temp("compile-v2-planned");
        let source = root.join("route.geojson");
        std::fs::write(
            &source,
            br#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"local","properties":{"height":12},"geometry":{"type":"Polygon","coordinates":[[[-87.623177,41.881832],[-87.623077,41.881832],[-87.623077,41.881932],[-87.623177,41.881932],[-87.623177,41.881832]]]}},{"type":"Feature","id":"outside-halo","properties":{"height":12},"geometry":{"type":"Polygon","coordinates":[[[-87.593177,41.881832],[-87.593077,41.881832],[-87.593077,41.881932],[-87.593177,41.881932],[-87.593177,41.881832]]]}}]}"#,
        )
        .unwrap();
        let policy_path = root.join("policy.json");
        let policy = GradedProbePolicy::new(
            [0, 0],
            vec![fightbox_world::ProbeTierPolicy {
                id: "residual".to_owned(),
                ground_up_mm: 1_500,
                ground_spacing_m: 32,
                analysis_spacing_m: 32,
                regions_city_enu_mm: vec![fightbox_world::CityBoundsMm::new(
                    [-1_000_000, -1_000_000],
                    [1_000_000, 1_000_000],
                )],
                elevated_layers: vec![fightbox_world::ElevatedProbeLayerPolicy {
                    id: "residual-63m".to_owned(),
                    up_mm: 63_000,
                    spacing_m: 32,
                }],
            }],
        );
        std::fs::write(&policy_path, serde_json::to_vec_pretty(&policy).unwrap()).unwrap();
        let package = root.join("planned.fightbox");
        compile_geojson_v2(
            &source,
            &package,
            CityCompileV2Config {
                city_id: "chicago-loop".to_owned(),
                geodetic_origin: GeodeticOrigin::wgs84(41.881_832, -87.623_177, 181.0),
                cell_grid_index: CellGridIndex { east: 0, north: 0 },
                probe_policy: Some(policy_path),
            },
        )
        .unwrap();

        let loaded = fightbox_world::read_package_with_capabilities(
            &package,
            &[fightbox_world::CITY_BAKE_V2_CAPABILITY],
        )
        .unwrap();
        assert_eq!(loaded.manifest.building_count, 1);
        assert_eq!(loaded.manifest.extensions.len(), 1);
        let sidecar =
            std::fs::read(package.join(fightbox_world::CITY_BAKE_V2_SIDECAR_PATH)).unwrap();
        let plan = fightbox_world::CityBakeV2ProbePlan::from_sidecar_bytes(&sidecar).unwrap();
        assert_eq!(plan.cell.grid_index, CellGridIndex { east: 0, north: 0 });
        assert!(!plan.probes.is_empty());
        assert_eq!(plan.to_sidecar_bytes().unwrap(), sidecar);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn oracle_compile_keeps_city_enu_and_does_not_claim_mobile_cell_identity() {
        let root = temp("compile-oracle");
        let source = root.join("oracle.geojson");
        std::fs::write(
            &source,
            br#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"local","properties":{"height":12},"geometry":{"type":"Polygon","coordinates":[[[-87.623177,41.881832],[-87.623077,41.881832],[-87.623077,41.881932],[-87.623177,41.881932],[-87.623177,41.881832]]]}},{"type":"Feature","id":"outside","properties":{"height":12},"geometry":{"type":"Polygon","coordinates":[[[-87.593177,41.881832],[-87.593077,41.881832],[-87.593077,41.881932],[-87.593177,41.881932],[-87.593177,41.881832]]]}}]}"#,
        )
        .unwrap();
        let package = root.join("oracle.fightbox");
        compile_geojson_oracle(
            &source,
            &package,
            &GeodeticOrigin::wgs84(41.881_832, -87.623_177, 181.0),
            CityBoundsMm::new([-100_000, -100_000], [100_000, 100_000]),
        )
        .unwrap();
        let loaded = read_package(&package).unwrap();
        assert_eq!(loaded.manifest.building_count, 1);
        assert_eq!(loaded.manifest.format_version, 1);
        assert!(loaded.manifest.world.is_none());
        assert!(
            loaded
                .mesh
                .vertices_enu_m
                .iter()
                .any(|vertex| { vertex.east_m.abs() < 1.0e-4 && vertex.north_m.abs() < 1.0e-4 })
        );
        assert!(
            loaded.mesh.vertices_enu_m.iter().any(|vertex| {
                (8.0..9.0).contains(&vertex.east_m) && vertex.north_m.abs() < 0.01
            })
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn synth_is_deterministic_and_round_trips_through_city_compile() {
        let first = synth_geojson(1, (6, 6)).unwrap();
        let second = synth_geojson(1, (6, 6)).unwrap();
        assert_eq!(first, second);
        assert_ne!(first, synth_geojson(2, (6, 6)).unwrap());
        let root_value: Value = serde_json::from_str(&first).unwrap();
        let features = root_value["features"].as_array().unwrap();
        assert!((100..=144).contains(&features.len()));
        assert!(features.iter().all(|feature| {
            let properties = feature["properties"].as_object().unwrap();
            properties.contains_key("height") || properties.contains_key("building:levels")
        }));

        let root = temp("synth-round-trip");
        let source = root.join("megablock.geojson");
        std::fs::write(&source, first).unwrap();
        let package = root.join("megablock.fightbox");
        compile_geojson(&source, &package).unwrap();
        let inspection = inspect_text(&package).unwrap();
        assert!(inspection.contains(&format!("buildings: {}", features.len())));
        assert!(inspection.contains("assumptions:\n  (none)"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn assumed_height_jitter_clamps_to_walkable_story() {
        assert_eq!(jitter_heights(4.0), [3.0, 4.0, 7.2]);
        assert_eq!(jitter_heights(3.0), [3.0, 3.0, 6.2]);
    }

    #[test]
    fn default_bake_config_preserves_the_legacy_probe_volume() {
        let mesh = SceneMesh {
            vertices_enu_m: vec![
                fightbox_steam_audio::EnuVector3::new(-2.0, -3.0, 0.0),
                fightbox_steam_audio::EnuVector3::new(4.0, 5.0, 60.0),
            ],
            triangles: Vec::new(),
            material_indices: Vec::new(),
            materials: Vec::new(),
        };
        let probes = probe_volume(&mesh, &BakeConfig::default()).unwrap();
        assert_eq!(
            probes,
            ProbeVolume {
                min_enu_m: fightbox_steam_audio::EnuVector3::new(-2.0, -3.0, 0.0),
                max_enu_m: fightbox_steam_audio::EnuVector3::new(4.0, 5.0, 3.0),
                spacing_m: 4.0,
                height_above_floor_m: 1.5,
            }
        );
        assert!(BakeConfig::default().elevated_probe_layers_m.is_empty());
        assert!(BakeConfig::default().elevated_probe_spacing_m.is_none());
    }

    #[test]
    fn city_bake_manifest_records_the_effective_bake_configuration() {
        let config = BakeConfig {
            path_range_m: 600.0,
            visibility_range_m: 20.0,
            visibility_samples: 4,
            visibility_threshold: 0.25,
            probe_spacing_m: 8.0,
            probe_height_above_floor_m: 3.0,
            probe_ceiling_m: 63.0,
            elevated_probe_layers_m: vec![30.0, 80.0],
            elevated_probe_spacing_m: Some(16.0),
            bake_threads: 10,
        };
        let manifest = city_bake_manifest("materials", "mesh", "probes", &config, 12.5);

        assert_eq!(manifest["materials_content_sha256"], "materials");
        assert_eq!(manifest["mesh_content_sha256"], "mesh");
        assert_eq!(manifest["probe_batch_sha256"], "probes");
        assert_eq!(manifest["bake_duration_s"], 12.5);
        assert_eq!(
            manifest["bake_config"],
            json!({
                "probe_ceiling_m": 63.0,
                "floor_probe_spacing_m": 8.0,
                "probe_height_above_floor_m": 3.0,
                "elevated_probe_layers": [
                    {"height_enu_m": 30.0, "spacing_m": 16.0},
                    {"height_enu_m": 80.0, "spacing_m": 16.0},
                ],
                "path_range_m": 600.0,
                "visibility_range_m": 20.0,
                "visibility_samples": 4,
                "visibility_threshold": 0.25,
                "threads": 10,
            })
        );
    }

    #[test]
    fn artifact_estimator_uses_the_unculled_layout_as_its_probe_bound() {
        let probes = ProbeVolume {
            min_enu_m: fightbox_steam_audio::EnuVector3::new(-5.0, -5.0, 0.0),
            max_enu_m: fightbox_steam_audio::EnuVector3::new(52.0, 30.0, 3.0),
            spacing_m: 4.0,
            height_above_floor_m: 1.5,
        };
        let estimate = estimate_bake_artifact(probes, &[], 100.0).unwrap();
        assert_eq!(estimate.probe_count_upper_bound, 135);
        assert_eq!(estimate.probe_batch_bytes, 216_810);
        assert_eq!(estimate.bytes, 282_346);
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn synthetic_default_bake_matches_expectation_and_estimator_bounds() {
        let root = bake_temp("artifact-estimate-real-bake");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/city/synthetic/block.geojson");
        let package = root.join("block.fightbox");
        compile_geojson(&source, &package).unwrap();

        let config = BakeConfig::default();
        let loaded = read_package(&package).unwrap();
        let scene = scene_mesh(&loaded).unwrap();
        let probes = probe_volume(&scene, &config).unwrap();
        let layers = elevated_probe_layers(&config);
        let estimate = estimate_bake_artifact(probes, &layers, config.path_range_m).unwrap();

        let output = root.join("block.baked");
        bake_with_config(&package, &output, config).unwrap();
        let baked = load_baked(&output).unwrap();
        let expected = crate::bake_expectations::expectation("synthetic-block-default");
        assert_eq!(
            (
                baked.metadata.content_sha256.as_str(),
                baked.metadata.probe_count
            ),
            (expected.artifact_sha256, expected.probe_count),
            "synthetic city bake differs from the checked-in expectation: either you changed \
             bake behavior intentionally (update bake_expectations.rs) or you broke determinism"
        );
        let actual_artifact_bytes: u64 = output
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(estimate.probe_count_upper_bound >= u64::from(baked.metadata.probe_count));
        assert!(estimate.bytes >= actual_artifact_bytes);
        assert!(estimate.bytes <= actual_artifact_bytes * 4);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Positions are a pure function of `(volume, layer, mesh)`, and neither the
    /// volume nor the mesh depends on the elevated spacing, so proving the
    /// layers are unchanged proves the probe positions are too — without paying
    /// for a rebake.
    #[test]
    fn unset_elevated_spacing_reproduces_the_floor_spaced_layers() {
        let config = BakeConfig {
            probe_spacing_m: 8.0,
            elevated_probe_layers_m: vec![30.0, 63.0],
            elevated_probe_spacing_m: None,
            ..BakeConfig::default()
        };
        assert_eq!(
            elevated_probe_layers(&config),
            vec![
                ElevatedProbeLayer {
                    height_enu_m: 30.0,
                    spacing_m: 8.0,
                },
                ElevatedProbeLayer {
                    height_enu_m: 63.0,
                    spacing_m: 8.0,
                },
            ]
        );
        // An empty layer list stays empty whatever the spacing says, which is
        // what keeps a no-flag bake byte-identical.
        let none = BakeConfig {
            elevated_probe_spacing_m: Some(16.0),
            ..BakeConfig::default()
        };
        assert!(elevated_probe_layers(&none).is_empty());
    }

    #[test]
    fn elevated_spacing_overrides_the_floor_spacing_for_every_layer() {
        let config = BakeConfig {
            probe_spacing_m: 8.0,
            elevated_probe_layers_m: vec![30.0, 63.0],
            elevated_probe_spacing_m: Some(16.0),
            ..BakeConfig::default()
        };
        let layers = elevated_probe_layers(&config);
        assert_eq!(layers.len(), 2);
        assert!(layers.iter().all(|layer| layer.spacing_m == 16.0));
        assert_eq!(layers[0].height_enu_m, 30.0);
        assert_eq!(layers[1].height_enu_m, 63.0);
    }

    #[test]
    fn chicago_percept_geometry_has_reserv_as_sole_occluder_at_every_height() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/city/chicago-block/chicago-block.geojson");
        let input = std::fs::read(source).unwrap();
        let normalized = normalize_geojson_coordinates(&input).unwrap();
        let root: Value = serde_json::from_slice(&normalized).unwrap();
        let footprint = feature_footprint(&root, OCCLUDER_ID).unwrap();
        for height in jitter_heights(DEFAULT_HEIGHT_M) {
            let mut variant = root.clone();
            set_feature_height(&mut variant, OCCLUDER_ID, height).unwrap();
            let bytes = serde_json::to_vec(&variant).unwrap();
            let mesh = compile(
                &city_provider(&bytes),
                &MaterialTable::default(),
                CompileOptions::default(),
            )
            .unwrap();
            let proof = prove_percept_geometry(&mesh, &footprint).unwrap();
            assert_eq!(proof.shadow_intersections, 2);
            assert_eq!(proof.shadow_target_intersections, 2);
            assert_eq!(proof.clear_intersections, 0);
            assert!(proof.sole_occluder_proved);
        }
    }
}
