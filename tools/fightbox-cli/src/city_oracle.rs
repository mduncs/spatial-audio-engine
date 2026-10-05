//! Monolithic desktop oracle assembly and explicit-position bake.
//!
//! This lane is deliberately separate from `city bake-v2`: it consumes the
//! same global graded policy but never assigns mobile-cell identity or the
//! phone's 600 m path horizon to the 1.17 km oracle.

use std::path::{Path, PathBuf};
use std::time::Instant;

use fightbox_evidence::sha256_hex;
use fightbox_steam_audio::{
    EXPLICIT_PROBE_BAKER_REVISION, ExplicitProbe, ExplicitProbeBakeRequest,
    PROBE_BATCH_METADATA_SCHEMA, PathBakeConfig, STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION,
    bake_explicit_probe_batch,
};
use fightbox_world::{
    CITY_BAKE_V2_CAPABILITY, CityBakeV2DeterministicTelemetry, CityBakeV2PathBakeSettings,
    CityBakeV2ProbeBatchIdentity, CityBoundsMm, CityRouteCellInput, CityRouteManifest,
    GradedProbePolicy, PackageCompression, ResolvedGradedProbeLayout,
    STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY, reachable_ordered_probe_pair_count,
    read_package_with_capabilities,
};
use serde::{Deserialize, Serialize};

use crate::atomicio::{
    AtomicDir, validate_output_path, write_bytes_atomic, write_json_string_atomic,
};
use crate::bake_reservation::{BakeReservation, format_bytes};
use crate::error::{CliError, Result};
use crate::probe_byte_estimate_v2::{
    PROBE_BYTE_ESTIMATE_V2_FILENAME, PROBE_BYTE_ESTIMATE_V2_SCHEMA, ProbeByteEstimateModelV2,
    ProbeByteEstimateObservedV2, ProbeByteEstimatePairCountKindV2, ProbeByteEstimatePathingV2,
    ProbeByteEstimateRequestV2, ProbeByteEstimateSdkV2, ProbeByteEstimateSubjectV2,
    ProbeByteEstimateV2, request_sha256,
};

const ORACLE_SCHEMA: &str = "fightbox.city-oracle.v1";
const ORACLE_PLAN_SCHEMA: &str = "fightbox.city-oracle-probe-plan.v1";
const ORACLE_MANIFEST_FILENAME: &str = "city-oracle-manifest.json";
const ORACLE_PLAN_FILENAME: &str = "city-oracle-probe-plan.json";
const ROUTE_COPY_FILENAME: &str = "source-city-route-manifest.json";
const ORACLE_PACKAGE_PATH: &str = "oracle.fightbox";
const ORACLE_CALIBRATION_ONLY_FILENAME: &str = "oracle-calibration-only.json";
const ORACLE_CALIBRATION_ONLY_SCHEMA: &str = "fightbox.city-oracle-calibration-only.v1";
const ORACLE_PATH_RANGE_M: u32 = 1_750;
const SERIALIZATION_FIXED_BYTES: u64 = 64 * 1_024;
const PROBE_BYTES: u64 = 256;
const REACHABLE_ORDERED_PAIR_BYTES: u64 = 10;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OracleBakeConfig {
    pub geojson: PathBuf,
    pub probe_policy: PathBuf,
    pub route_manifest: PathBuf,
    pub cell_packages: Vec<PathBuf>,
    pub cell_bakes: Vec<PathBuf>,
    pub output: PathBuf,
    pub visibility_samples: i32,
    pub probe_visibility_radius_m: f32,
    pub visibility_threshold: f32,
    pub visibility_range_m: f32,
    pub bake_threads: i32,
    pub expected_probe_count: Option<u64>,
    pub policy_independent_calibration: bool,
}

impl OracleBakeConfig {
    pub(crate) fn with_defaults(
        geojson: PathBuf,
        probe_policy: PathBuf,
        route_manifest: PathBuf,
        cell_packages: Vec<PathBuf>,
        cell_bakes: Vec<PathBuf>,
        output: PathBuf,
    ) -> Self {
        let defaults = PathBakeConfig::default();
        Self {
            geojson,
            probe_policy,
            route_manifest,
            cell_packages,
            cell_bakes,
            output,
            visibility_samples: defaults.num_visibility_samples,
            probe_visibility_radius_m: defaults.probe_visibility_radius_m,
            visibility_threshold: defaults.visibility_threshold,
            visibility_range_m: defaults.visibility_range_m,
            bake_threads: defaults.num_threads,
            expected_probe_count: None,
            policy_independent_calibration: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OracleCalibrationOnly {
    schema_version: String,
    artifact_state: String,
    production_eligible: bool,
    source_route_placement_policy_sha256: String,
    calibration_placement_policy_sha256: String,
    expected_probe_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OracleByteEstimate {
    estimator_revision: String,
    path_horizon_m: u32,
    probe_count: u64,
    reachable_ordered_pair_count: u64,
    estimated_raw_bytes: u64,
    projected_low_bytes: u64,
    projected_high_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OracleProbePlan {
    schema_version: String,
    artifact_state: String,
    coordinate_frame: String,
    coordinate_encoding: String,
    path_horizon_m: u32,
    layout: ResolvedGradedProbeLayout,
    byte_estimate: OracleByteEstimate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OracleInputBinding {
    route_manifest_sha256: String,
    route_id: String,
    city_id: String,
    cell_ids: Vec<String>,
    source_geojson_sha256: String,
    source_probe_policy_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OraclePackageBinding {
    relative_path: String,
    manifest_sha256: String,
    mesh_sha256: String,
    materials_sha256: String,
    triangle_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OracleManifest {
    schema_version: String,
    artifact_state: String,
    oracle_bounds_city_enu_mm: CityBoundsMm,
    geometry_bounds_city_enu_mm: CityBoundsMm,
    path_range_m: u32,
    input: OracleInputBinding,
    probe_plan_path: String,
    probe_plan_sha256: String,
    placement_policy_sha256: String,
    probe_layout_sha256: String,
    byte_estimate: OracleByteEstimate,
    package: OraclePackageBinding,
    pathing: CityBakeV2PathBakeSettings,
    probe_batch: CityBakeV2ProbeBatchIdentity,
    deterministic_telemetry: CityBakeV2DeterministicTelemetry,
    bake_duration_s: f64,
    reservation_bytes: u64,
}

pub(crate) fn bake(config: OracleBakeConfig) -> Result<()> {
    crate::city::require_linked("city oracle-bake")?;
    if config.cell_packages.len() != 4 || config.cell_bakes.len() != 4 {
        return Err(CliError::new(
            "city oracle-bake requires exactly four --cell-package and four --cell-bake arguments",
        ));
    }

    let route_bytes = read_file(&config.route_manifest, "city route manifest")?;
    let route = CityRouteManifest::from_bytes(&route_bytes)
        .map_err(|error| CliError::new(format!("invalid city route manifest: {error}")))?;
    if route
        .to_bytes()
        .map_err(|error| CliError::new(format!("cannot canonicalize city route: {error}")))?
        != route_bytes
    {
        return Err(CliError::new(
            "city oracle-bake requires canonical city-route planner bytes",
        ));
    }
    let fixture = route.four_cell_fixture.as_ref().ok_or_else(|| {
        CliError::new("city oracle-bake requires the route's four-cell fixture plan")
    })?;
    if fixture.state != "streamed_cell_bakes_complete_oracle_pending"
        || !fixture.bakes_launched
        || fixture.streamed_cell_bakes_required != 4
        || fixture.monolithic_oracle_bakes_required != 1
        || fixture.monolithic_oracle_path_range_m != ORACLE_PATH_RANGE_M
    {
        return Err(CliError::new(
            "four-cell route must contain four completed streamed bakes with exactly one 1,750 m oracle pending",
        ));
    }

    let mut inputs = Vec::with_capacity(4);
    let mut geodetic_origin = None;
    for (package, baked) in config.cell_packages.iter().zip(&config.cell_bakes) {
        inputs.push(crate::city_route::load_cell(package, Some(baked), true)?);
        let loaded = read_package_with_capabilities(package, &[CITY_BAKE_V2_CAPABILITY]).map_err(
            |error| {
                CliError::new(format!(
                    "cannot load oracle source cell {}: {error}",
                    package.display()
                ))
            },
        )?;
        let origin = loaded
            .manifest
            .world
            .as_ref()
            .ok_or_else(|| CliError::new("oracle source package is not world-manifest-v2"))?
            .city
            .geodetic_origin
            .clone();
        match &geodetic_origin {
            None => geodetic_origin = Some(origin),
            Some(expected) if expected == &origin => {}
            Some(_) => {
                return Err(CliError::new(
                    "oracle source cells disagree on geodetic origin",
                ));
            }
        }
    }
    verify_oracle_source_cells(&route, &inputs)?;

    let policy_bytes = read_file(&config.probe_policy, "graded probe policy")?;
    let policy: GradedProbePolicy = serde_json::from_slice(&policy_bytes)
        .map_err(|error| CliError::new(format!("invalid graded probe policy: {error}")))?;
    let layout = policy
        .resolve_bounds(fixture.monolithic_oracle_bounds_city_enu_mm)
        .map_err(|error| CliError::new(format!("cannot resolve oracle probe layout: {error}")))?;
    let byte_estimate = estimate_oracle_bytes(&layout)?;
    if let Some(expected) = config.expected_probe_count
        && byte_estimate.probe_count != expected
    {
        return Err(CliError::new(format!(
            "city oracle-bake expected exactly {expected} probes but the resolved policy produced {}",
            byte_estimate.probe_count
        )));
    }
    let source_route_policy_sha256 = route
        .cells
        .first()
        .ok_or_else(|| CliError::new("oracle source route contains no cells"))?
        .city_bake
        .placement_policy_sha256
        .clone();
    if route
        .cells
        .iter()
        .any(|cell| cell.city_bake.placement_policy_sha256 != source_route_policy_sha256)
    {
        return Err(CliError::new(
            "oracle source route cells disagree on placement policy",
        ));
    }
    let calibration_marker_bytes = if config.policy_independent_calibration {
        let expected_probe_count = config.expected_probe_count.ok_or_else(|| {
            CliError::new(
                "--policy-independent-calibration requires --expected-probe-count before SDK work",
            )
        })?;
        Some(canonical_json_bytes(&OracleCalibrationOnly {
            schema_version: ORACLE_CALIBRATION_ONLY_SCHEMA.to_owned(),
            artifact_state: "calibration_only".to_owned(),
            production_eligible: false,
            source_route_placement_policy_sha256: source_route_policy_sha256.clone(),
            calibration_placement_policy_sha256: layout.placement_policy_sha256.clone(),
            expected_probe_count,
        })?)
    } else {
        if layout.placement_policy_sha256 != source_route_policy_sha256 {
            return Err(CliError::new(
                "oracle policy differs from the source route; use the explicit policy-independent calibration lane only for non-production diagnostics",
            ));
        }
        None
    };
    let probe_plan = OracleProbePlan {
        schema_version: ORACLE_PLAN_SCHEMA.to_owned(),
        artifact_state: "probe_plan".to_owned(),
        coordinate_frame: "city_enu_m".to_owned(),
        coordinate_encoding: "signed_millimetres".to_owned(),
        path_horizon_m: ORACLE_PATH_RANGE_M,
        layout,
        byte_estimate: byte_estimate.clone(),
    };
    let probe_plan_bytes = canonical_json_bytes(&probe_plan)?;

    let output = validate_output_path(&config.output)?;
    let directory = AtomicDir::create(output.clone())?;
    let temp = directory.temp_path();
    write_bytes_atomic(&temp.join(ROUTE_COPY_FILENAME), &route_bytes)?;
    write_bytes_atomic(&temp.join(ORACLE_PLAN_FILENAME), &probe_plan_bytes)?;
    if let Some(bytes) = &calibration_marker_bytes {
        write_bytes_atomic(&temp.join(ORACLE_CALIBRATION_ONLY_FILENAME), bytes)?;
    }

    let geometry_bounds = expand_bounds(
        fixture.monolithic_oracle_bounds_city_enu_mm,
        i64::from(route.grid_policy.geometry_halo_m) * 1_000,
    )?;
    let package_path = temp.join(ORACLE_PACKAGE_PATH);
    crate::city::compile_geojson_oracle(
        &config.geojson,
        &package_path,
        geodetic_origin
            .as_ref()
            .expect("four source cells provide an origin"),
        geometry_bounds,
    )?;
    let package = fightbox_world::read_package(&package_path)
        .map_err(|error| CliError::new(format!("cannot reload oracle package: {error}")))?;
    let package_manifest_bytes = read_file(
        &package_path.join("manifest.json"),
        "oracle package manifest",
    )?;

    let probes = probe_plan
        .layout
        .probes
        .iter()
        .map(|probe| {
            ExplicitProbe::new(
                fightbox_steam_audio::EnuVector3::new(
                    probe.center_city_enu_mm[0] as f32 / 1_000.0,
                    probe.center_city_enu_mm[1] as f32 / 1_000.0,
                    probe.center_city_enu_mm[2] as f32 / 1_000.0,
                ),
                probe.radius_mm as f32 / 1_000.0,
            )
        })
        .collect::<Vec<_>>();
    let pathing = PathBakeConfig {
        num_visibility_samples: config.visibility_samples,
        probe_visibility_radius_m: config.probe_visibility_radius_m,
        visibility_threshold: config.visibility_threshold,
        visibility_range_m: config.visibility_range_m,
        path_range_m: ORACLE_PATH_RANGE_M as f32,
        num_threads: config.bake_threads,
    };
    let reservation = BakeReservation::create(
        &temp.join("probe-batch.bin"),
        &output,
        byte_estimate.projected_high_bytes,
    )?;
    eprintln!(
        "fightbox: city oracle-bake reserved {} for {} exact probes and {} reachable ordered pairs (filesystem reported {} available)",
        format_bytes(byte_estimate.projected_high_bytes),
        byte_estimate.probe_count,
        byte_estimate.reachable_ordered_pair_count,
        format_bytes(reservation.reported_available_bytes()),
    );
    let started = Instant::now();
    let baked = bake_explicit_probe_batch(&ExplicitProbeBakeRequest {
        mesh: crate::city::scene_mesh(&package)?,
        probes,
        pathing,
    })
    .map_err(|error| CliError::new(format!("monolithic oracle bake failed: {error}")))?;
    let bake_duration_s = started.elapsed().as_secs_f64();
    baked.validate().map_err(|error| {
        CliError::new(format!("monolithic oracle probe batch is invalid: {error}"))
    })?;
    if u64::from(baked.metadata.probe_count) != byte_estimate.probe_count {
        return Err(CliError::new(
            "monolithic oracle serialized probe count differs from its exact plan",
        ));
    }
    if baked.metadata.serialized_size_bytes > byte_estimate.projected_high_bytes {
        return Err(CliError::new(format!(
            "oracle batch {} exceeds its reserved high estimate {}",
            format_bytes(baked.metadata.serialized_size_bytes),
            format_bytes(byte_estimate.projected_high_bytes),
        )));
    }
    let metadata_json = baked.metadata.to_json();
    let source_geojson_bytes = read_file(&config.geojson, "source GeoJSON")?;
    let manifest = OracleManifest {
        schema_version: ORACLE_SCHEMA.to_owned(),
        artifact_state: "monolithic_oracle_bake_complete".to_owned(),
        oracle_bounds_city_enu_mm: fixture.monolithic_oracle_bounds_city_enu_mm,
        geometry_bounds_city_enu_mm: geometry_bounds,
        path_range_m: ORACLE_PATH_RANGE_M,
        input: OracleInputBinding {
            route_manifest_sha256: sha256_hex(&route_bytes),
            route_id: route.route_id.clone(),
            city_id: route.city_id.clone(),
            cell_ids: route
                .cells
                .iter()
                .map(|cell| cell.cell_id.clone())
                .collect(),
            source_geojson_sha256: sha256_hex(&source_geojson_bytes),
            source_probe_policy_sha256: sha256_hex(&policy_bytes),
        },
        probe_plan_path: ORACLE_PLAN_FILENAME.to_owned(),
        probe_plan_sha256: sha256_hex(&probe_plan_bytes),
        placement_policy_sha256: probe_plan.layout.placement_policy_sha256.clone(),
        probe_layout_sha256: probe_plan.layout.probe_layout_sha256.clone(),
        byte_estimate: byte_estimate.clone(),
        package: OraclePackageBinding {
            relative_path: ORACLE_PACKAGE_PATH.to_owned(),
            manifest_sha256: sha256_hex(&package_manifest_bytes),
            mesh_sha256: package.manifest.mesh_content_sha256.clone(),
            materials_sha256: package.manifest.materials_content_sha256.clone(),
            triangle_count: u64::try_from(package.manifest.triangle_count)
                .map_err(|_| CliError::new("oracle triangle count exceeds u64"))?,
        },
        pathing: CityBakeV2PathBakeSettings {
            num_visibility_samples: pathing.num_visibility_samples,
            probe_visibility_radius_m: pathing.probe_visibility_radius_m,
            visibility_threshold: pathing.visibility_threshold,
            visibility_range_m: pathing.visibility_range_m,
            path_range_m: pathing.path_range_m,
            num_threads: pathing.num_threads,
        },
        probe_batch: CityBakeV2ProbeBatchIdentity {
            capability: STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY.to_owned(),
            payload_path: "probe-batch.bin".to_owned(),
            compression: PackageCompression::None,
            metadata_schema: PROBE_BATCH_METADATA_SCHEMA.to_owned(),
            steam_audio_version: STEAM_AUDIO_VERSION.to_owned(),
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT.to_owned(),
            probe_count: u64::from(baked.metadata.probe_count),
            path_data_size_bytes: baked.metadata.path_data_size_bytes,
            serialized_size_bytes: baked.metadata.serialized_size_bytes,
            serialized_sha256: baked.metadata.content_sha256.clone(),
        },
        deterministic_telemetry: CityBakeV2DeterministicTelemetry {
            baker_revision: EXPLICIT_PROBE_BAKER_REVISION.to_owned(),
            submitted_explicit_probe_count: byte_estimate.probe_count,
            committed_probe_count: u64::from(baked.metadata.probe_count),
            insertion_order: "oracle_probe_plan_city_enu_centres".to_owned(),
            bake_progress_callback_count: baked.metadata.bake_progress_callback_count,
            final_bake_progress_millionths: baked.metadata.final_bake_progress_millionths,
        },
        bake_duration_s,
        reservation_bytes: byte_estimate.projected_high_bytes,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| CliError::new(format!("cannot serialize oracle manifest: {error}")))?;
    let actual_payload_bytes = u64::try_from(baked.bytes.len())
        .map_err(|_| CliError::new("oracle probe batch exceeds u64"))?;
    let metadata_bytes = u64::try_from(metadata_json.len())
        .map_err(|_| CliError::new("oracle metadata length exceeds u64"))?;
    let manifest_size = u64::try_from(manifest_bytes.len())
        .map_err(|_| CliError::new("oracle manifest length exceeds u64"))?;
    let route_size = u64::try_from(route_bytes.len())
        .map_err(|_| CliError::new("oracle route copy length exceeds u64"))?;
    let plan_size = u64::try_from(probe_plan_bytes.len())
        .map_err(|_| CliError::new("oracle probe plan length exceeds u64"))?;
    let calibration_marker_size =
        u64::try_from(calibration_marker_bytes.as_ref().map_or(0, Vec::len))
            .map_err(|_| CliError::new("oracle calibration marker length exceeds u64"))?;
    let package_bytes = directory_file_bytes(&package_path)?;
    let artifact_bytes_without_estimate = actual_payload_bytes
        .checked_add(metadata_bytes)
        .and_then(|bytes| bytes.checked_add(manifest_size))
        .and_then(|bytes| bytes.checked_add(route_size))
        .and_then(|bytes| bytes.checked_add(plan_size))
        .and_then(|bytes| bytes.checked_add(calibration_marker_size))
        .and_then(|bytes| bytes.checked_add(package_bytes))
        .ok_or_else(|| CliError::new("oracle aggregate artifact bytes overflow u64"))?;
    let mut estimate_request = ProbeByteEstimateRequestV2 {
        request_sha256: String::new(),
        mesh_sha256: package.manifest.mesh_content_sha256.clone(),
        materials_sha256: package.manifest.materials_content_sha256.clone(),
        probe_plan_sha256: Some(sha256_hex(&probe_plan_bytes)),
        placement_policy_sha256: Some(probe_plan.layout.placement_policy_sha256.clone()),
        probe_layout_sha256: Some(probe_plan.layout.probe_layout_sha256.clone()),
        probe_count: byte_estimate.probe_count,
        path_horizon_m: ORACLE_PATH_RANGE_M,
        pathing: ProbeByteEstimatePathingV2 {
            visibility_range_m: pathing.visibility_range_m,
            visibility_samples: pathing.num_visibility_samples,
            visibility_threshold: pathing.visibility_threshold,
            probe_visibility_radius_m: pathing.probe_visibility_radius_m,
            threads: pathing.num_threads,
        },
        sdk: ProbeByteEstimateSdkV2 {
            metadata_schema: PROBE_BATCH_METADATA_SCHEMA.to_owned(),
            steam_audio_version: STEAM_AUDIO_VERSION.to_owned(),
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT.to_owned(),
            baker_revision: EXPLICIT_PROBE_BAKER_REVISION.to_owned(),
        },
    };
    estimate_request.request_sha256 = request_sha256(&estimate_request).map_err(|error| {
        CliError::new(format!("cannot identify oracle estimate request: {error}"))
    })?;
    let estimate = ProbeByteEstimateV2 {
        schema_version: PROBE_BYTE_ESTIMATE_V2_SCHEMA.to_owned(),
        artifact_state: "completed".to_owned(),
        subject: ProbeByteEstimateSubjectV2 {
            schema_version: ORACLE_SCHEMA.to_owned(),
            manifest_path: ORACLE_MANIFEST_FILENAME.to_owned(),
            manifest_sha256: sha256_hex(&manifest_bytes),
            package_manifest_sha256: sha256_hex(&package_manifest_bytes),
        },
        request: estimate_request,
        model: ProbeByteEstimateModelV2 {
            revision: byte_estimate.estimator_revision.clone(),
            probe_bytes: PROBE_BYTES,
            pair_bytes: REACHABLE_ORDERED_PAIR_BYTES,
            fixed_bytes: SERIALIZATION_FIXED_BYTES,
            pair_count: byte_estimate.reachable_ordered_pair_count,
            pair_count_kind: ProbeByteEstimatePairCountKindV2::Exact,
            estimated_raw_bytes: byte_estimate.estimated_raw_bytes,
            projected_low_bytes: byte_estimate.projected_low_bytes,
            projected_high_bytes: byte_estimate.projected_high_bytes,
            reservation_bytes: reservation.reserved_bytes(),
            calibrated_model: None,
        },
        observed: ProbeByteEstimateObservedV2 {
            probe_count: u64::from(baked.metadata.probe_count),
            path_data_size_bytes: baked.metadata.path_data_size_bytes,
            serialized_size_bytes: baked.metadata.serialized_size_bytes,
            payload_sha256: baked.metadata.content_sha256.clone(),
            artifact_bytes: 0,
        },
    };
    let (estimate, estimate_bytes) = estimate
        .finalize_canonical_json(artifact_bytes_without_estimate)
        .map_err(|error| {
            CliError::new(format!(
                "cannot finalize oracle probe-byte estimate: {error}"
            ))
        })?;
    let additional_artifact_bytes = estimate
        .observed
        .artifact_bytes
        .checked_sub(actual_payload_bytes)
        .ok_or_else(|| CliError::new("oracle estimate artifact bytes omit payload"))?;
    reservation.assert_post_bake(
        actual_payload_bytes,
        baked.metadata.serialized_size_bytes,
        additional_artifact_bytes,
    )?;
    reservation.finish(&baked.bytes)?;
    write_json_string_atomic(&temp.join("probe-batch-metadata.json"), &metadata_json)?;
    write_bytes_atomic(&temp.join(ORACLE_MANIFEST_FILENAME), &manifest_bytes)?;
    write_bytes_atomic(&temp.join(PROBE_BYTE_ESTIMATE_V2_FILENAME), &estimate_bytes)?;
    verify_artifact(temp)?;
    directory.commit()?;
    verify_artifact(&output)?;
    eprintln!(
        "fightbox: monolithic oracle written to {} (probes={}, serialized={}, sha256={})",
        output.display(),
        baked.metadata.probe_count,
        format_bytes(baked.metadata.serialized_size_bytes),
        baked.metadata.content_sha256,
    );
    Ok(())
}

pub(crate) fn verify_artifact(root: &Path) -> Result<()> {
    let estimate_path = root.join(PROBE_BYTE_ESTIMATE_V2_FILENAME);
    let calibration_marker_path = root.join(ORACLE_CALIBRATION_ONLY_FILENAME);
    if calibration_marker_path.exists() && !estimate_path.exists() {
        return Err(CliError::new(
            "oracle calibration-only artifact requires probe-byte-estimate-v2.json",
        ));
    }
    validate_oracle_artifact_layout(root, estimate_path.exists())?;
    let manifest_bytes = read_file(&root.join(ORACLE_MANIFEST_FILENAME), "oracle manifest")?;
    let manifest: OracleManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| CliError::new(format!("invalid oracle manifest: {error}")))?;
    if serde_json::to_vec_pretty(&manifest)
        .map_err(|error| CliError::new(format!("cannot canonicalize oracle manifest: {error}")))?
        != manifest_bytes
    {
        return Err(CliError::new(
            "oracle manifest is valid JSON but not canonical bytes",
        ));
    }
    if manifest.schema_version != ORACLE_SCHEMA
        || manifest.artifact_state != "monolithic_oracle_bake_complete"
        || manifest.path_range_m != ORACLE_PATH_RANGE_M
        || manifest.pathing.path_range_m.to_bits() != (ORACLE_PATH_RANGE_M as f32).to_bits()
        || manifest.probe_plan_path != ORACLE_PLAN_FILENAME
        || manifest.package.relative_path != ORACLE_PACKAGE_PATH
        || manifest.probe_batch.payload_path != "probe-batch.bin"
        || manifest.probe_batch.compression != PackageCompression::None
    {
        return Err(CliError::new(
            "oracle manifest has drifted frozen identity or paths",
        ));
    }

    let route_bytes = read_file(&root.join(ROUTE_COPY_FILENAME), "copied route manifest")?;
    let route = CityRouteManifest::from_bytes(&route_bytes)
        .map_err(|error| CliError::new(format!("invalid copied oracle route: {error}")))?;
    if route.to_bytes().map_err(|error| {
        CliError::new(format!("cannot canonicalize copied oracle route: {error}"))
    })? != route_bytes
    {
        return Err(CliError::new(
            "copied oracle route is valid JSON but not canonical bytes",
        ));
    }
    if sha256_hex(&route_bytes) != manifest.input.route_manifest_sha256
        || route.route_id != manifest.input.route_id
        || route.city_id != manifest.input.city_id
        || route
            .cells
            .iter()
            .map(|cell| cell.cell_id.clone())
            .collect::<Vec<_>>()
            != manifest.input.cell_ids
    {
        return Err(CliError::new(
            "oracle copied route differs from its manifest binding",
        ));
    }

    let plan_bytes = read_file(&root.join(ORACLE_PLAN_FILENAME), "oracle probe plan")?;
    if sha256_hex(&plan_bytes) != manifest.probe_plan_sha256 {
        return Err(CliError::new(
            "oracle probe-plan hash differs from its manifest",
        ));
    }
    let plan: OracleProbePlan = serde_json::from_slice(&plan_bytes)
        .map_err(|error| CliError::new(format!("invalid oracle probe plan: {error}")))?;
    if canonical_json_bytes(&plan)? != plan_bytes
        || plan.schema_version != ORACLE_PLAN_SCHEMA
        || plan.artifact_state != "probe_plan"
        || plan.path_horizon_m != ORACLE_PATH_RANGE_M
        || plan.layout.bounds_city_enu_mm != manifest.oracle_bounds_city_enu_mm
        || plan.layout.placement_policy_sha256 != manifest.placement_policy_sha256
        || plan.layout.probe_layout_sha256 != manifest.probe_layout_sha256
        || plan.byte_estimate != manifest.byte_estimate
        || estimate_oracle_bytes(&plan.layout)? != plan.byte_estimate
    {
        return Err(CliError::new(
            "oracle probe plan is non-canonical or cross-binding has drifted",
        ));
    }

    let source_route_policy_sha256 = route
        .cells
        .first()
        .ok_or_else(|| CliError::new("oracle copied route contains no cells"))?
        .city_bake
        .placement_policy_sha256
        .clone();
    if route
        .cells
        .iter()
        .any(|cell| cell.city_bake.placement_policy_sha256 != source_route_policy_sha256)
    {
        return Err(CliError::new(
            "oracle copied route cells disagree on placement policy",
        ));
    }
    if calibration_marker_path.exists() {
        let marker_bytes = read_file(&calibration_marker_path, "oracle calibration-only marker")?;
        let marker: OracleCalibrationOnly =
            serde_json::from_slice(&marker_bytes).map_err(|error| {
                CliError::new(format!("invalid oracle calibration marker: {error}"))
            })?;
        if canonical_json_bytes(&marker)? != marker_bytes
            || marker.schema_version != ORACLE_CALIBRATION_ONLY_SCHEMA
            || marker.artifact_state != "calibration_only"
            || marker.production_eligible
            || marker.source_route_placement_policy_sha256 != source_route_policy_sha256
            || marker.calibration_placement_policy_sha256 != plan.layout.placement_policy_sha256
            || marker.expected_probe_count != plan.byte_estimate.probe_count
        {
            return Err(CliError::new(
                "oracle calibration marker is noncanonical or differs from its route/plan",
            ));
        }
    } else if plan.layout.placement_policy_sha256 != source_route_policy_sha256 {
        return Err(CliError::new(
            "oracle policy differs from the source route without a calibration-only authority marker",
        ));
    }

    let package_path = root.join(ORACLE_PACKAGE_PATH);
    let package = fightbox_world::read_package(&package_path)
        .map_err(|error| CliError::new(format!("invalid oracle package: {error}")))?;
    let package_manifest_bytes = read_file(
        &package_path.join("manifest.json"),
        "oracle package manifest",
    )?;
    if sha256_hex(&package_manifest_bytes) != manifest.package.manifest_sha256
        || package.manifest.mesh_content_sha256 != manifest.package.mesh_sha256
        || package.manifest.materials_content_sha256 != manifest.package.materials_sha256
        || u64::try_from(package.manifest.triangle_count).ok()
            != Some(manifest.package.triangle_count)
    {
        return Err(CliError::new(
            "oracle package differs from its manifest binding",
        ));
    }

    let baked = crate::city::load_baked(root)?;
    let metadata_bytes = read_file(
        &root.join("probe-batch-metadata.json"),
        "oracle probe-batch metadata",
    )?;
    if baked.metadata.to_json().as_bytes() != metadata_bytes {
        return Err(CliError::new(
            "oracle probe-batch metadata is valid JSON but not canonical bytes",
        ));
    }
    if manifest.probe_batch.metadata_schema != PROBE_BATCH_METADATA_SCHEMA
        || manifest.probe_batch.steam_audio_version != STEAM_AUDIO_VERSION
        || manifest.probe_batch.upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT
        || manifest.probe_batch.capability != STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY
        || manifest.probe_batch.probe_count != u64::from(baked.metadata.probe_count)
        || manifest.probe_batch.path_data_size_bytes != baked.metadata.path_data_size_bytes
        || manifest.probe_batch.serialized_size_bytes != baked.metadata.serialized_size_bytes
        || manifest.probe_batch.serialized_sha256 != baked.metadata.content_sha256
        || manifest
            .deterministic_telemetry
            .submitted_explicit_probe_count
            != manifest.byte_estimate.probe_count
        || manifest.deterministic_telemetry.committed_probe_count
            != manifest.probe_batch.probe_count
        || manifest.deterministic_telemetry.baker_revision != EXPLICIT_PROBE_BAKER_REVISION
        || manifest.deterministic_telemetry.insertion_order != "oracle_probe_plan_city_enu_centres"
        || manifest
            .deterministic_telemetry
            .bake_progress_callback_count
            != baked.metadata.bake_progress_callback_count
        || manifest
            .deterministic_telemetry
            .final_bake_progress_millionths
            != baked.metadata.final_bake_progress_millionths
        || manifest.probe_batch.serialized_size_bytes > manifest.reservation_bytes
    {
        return Err(CliError::new(
            "oracle probe batch differs from its manifest binding",
        ));
    }

    let coverage = baked
        .probe_coverage()
        .map_err(|error| CliError::new(format!("cannot decode oracle probe coverage: {error}")))?;
    if coverage.probe_count() != plan.layout.probes.len()
        || !coverage
            .spheres()
            .zip(plan.layout.probes.iter())
            .all(|((center, radius), expected)| {
                center.x.to_bits() == (expected.center_city_enu_mm[0] as f32 / 1_000.0).to_bits()
                    && center.y.to_bits()
                        == (expected.center_city_enu_mm[1] as f32 / 1_000.0).to_bits()
                    && center.z.to_bits()
                        == (expected.center_city_enu_mm[2] as f32 / 1_000.0).to_bits()
                    && radius.to_bits() == (expected.radius_mm as f32 / 1_000.0).to_bits()
            })
    {
        return Err(CliError::new(
            "oracle serialized probe spheres differ from the exact ordered plan",
        ));
    }

    let estimate_path = root.join(PROBE_BYTE_ESTIMATE_V2_FILENAME);
    if estimate_path.exists() {
        let estimate_bytes = read_file(
            &root.join(PROBE_BYTE_ESTIMATE_V2_FILENAME),
            "oracle probe-byte estimate v2",
        )?;
        let estimate = ProbeByteEstimateV2::from_json(&estimate_bytes).map_err(|error| {
            CliError::new(format!("invalid oracle probe-byte estimate: {error}"))
        })?;
        if estimate.to_canonical_json().map_err(|error| {
            CliError::new(format!(
                "cannot canonicalize oracle probe-byte estimate: {error}"
            ))
        })? != estimate_bytes
        {
            return Err(CliError::new(
                "oracle probe-byte estimate is valid JSON but not canonical bytes",
            ));
        }
        let estimate_matches = estimate.subject.schema_version == ORACLE_SCHEMA
            && estimate.subject.manifest_sha256 == sha256_hex(&manifest_bytes)
            && estimate.subject.package_manifest_sha256 == sha256_hex(&package_manifest_bytes)
            && estimate.request.mesh_sha256 == package.manifest.mesh_content_sha256
            && estimate.request.materials_sha256 == package.manifest.materials_content_sha256
            && estimate.request.probe_plan_sha256.as_deref()
                == Some(manifest.probe_plan_sha256.as_str())
            && estimate.request.placement_policy_sha256.as_deref()
                == Some(manifest.placement_policy_sha256.as_str())
            && estimate.request.probe_layout_sha256.as_deref()
                == Some(manifest.probe_layout_sha256.as_str())
            && estimate.request.probe_count == manifest.byte_estimate.probe_count
            && estimate.request.path_horizon_m == ORACLE_PATH_RANGE_M
            && estimate.request.pathing.visibility_range_m.to_bits()
                == manifest.pathing.visibility_range_m.to_bits()
            && estimate.request.pathing.visibility_samples
                == manifest.pathing.num_visibility_samples
            && estimate.request.pathing.visibility_threshold.to_bits()
                == manifest.pathing.visibility_threshold.to_bits()
            && estimate.request.pathing.probe_visibility_radius_m.to_bits()
                == manifest.pathing.probe_visibility_radius_m.to_bits()
            && estimate.request.pathing.threads == manifest.pathing.num_threads
            && estimate.request.sdk.metadata_schema == manifest.probe_batch.metadata_schema
            && estimate.request.sdk.steam_audio_version == manifest.probe_batch.steam_audio_version
            && estimate.request.sdk.upstream_commit == manifest.probe_batch.upstream_commit
            && estimate.request.sdk.baker_revision
                == manifest.deterministic_telemetry.baker_revision
            && estimate.model.revision == manifest.byte_estimate.estimator_revision
            && estimate.model.pair_count == manifest.byte_estimate.reachable_ordered_pair_count
            && estimate.model.pair_count_kind == ProbeByteEstimatePairCountKindV2::Exact
            && estimate.model.estimated_raw_bytes == manifest.byte_estimate.estimated_raw_bytes
            && estimate.model.projected_low_bytes == manifest.byte_estimate.projected_low_bytes
            && estimate.model.projected_high_bytes == manifest.byte_estimate.projected_high_bytes
            && estimate.model.reservation_bytes == manifest.reservation_bytes
            && estimate.observed.probe_count == manifest.probe_batch.probe_count
            && estimate.observed.path_data_size_bytes == manifest.probe_batch.path_data_size_bytes
            && estimate.observed.serialized_size_bytes
                == manifest.probe_batch.serialized_size_bytes
            && estimate.observed.payload_sha256 == manifest.probe_batch.serialized_sha256
            && estimate.observed.artifact_bytes == directory_file_bytes(root)?;
        if !estimate_matches {
            return Err(CliError::new(
                "oracle probe-byte estimate differs from its manifest, package, plan, payload, or installed bytes",
            ));
        }
    }
    Ok(())
}

fn validate_oracle_artifact_layout(root: &Path, require_estimate: bool) -> Result<()> {
    let mut actual = std::fs::read_dir(root)
        .map_err(|error| CliError::new(format!("cannot inspect {}: {error}", root.display())))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| CliError::new(format!("cannot inspect oracle artifact: {error}")))?;
    actual.sort();
    let mut expected = vec![
        std::ffi::OsString::from(ORACLE_MANIFEST_FILENAME),
        std::ffi::OsString::from(ORACLE_PLAN_FILENAME),
        std::ffi::OsString::from(ORACLE_PACKAGE_PATH),
        std::ffi::OsString::from(ROUTE_COPY_FILENAME),
        std::ffi::OsString::from("probe-batch.bin"),
        std::ffi::OsString::from("probe-batch-metadata.json"),
    ];
    if require_estimate {
        expected.push(std::ffi::OsString::from(PROBE_BYTE_ESTIMATE_V2_FILENAME));
    }
    if root.join(ORACLE_CALIBRATION_ONLY_FILENAME).exists() {
        expected.push(std::ffi::OsString::from(ORACLE_CALIBRATION_ONLY_FILENAME));
    }
    expected.sort();
    if actual != expected {
        return Err(CliError::new(
            "oracle artifact contains missing or unbound root entries",
        ));
    }
    Ok(())
}

fn directory_file_bytes(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|error| {
            CliError::new(format!("cannot inspect {}: {error}", directory.display()))
        })? {
            let entry = entry.map_err(|error| {
                CliError::new(format!("cannot inspect oracle artifact: {error}"))
            })?;
            let file_type = entry.file_type().map_err(|error| {
                CliError::new(format!(
                    "cannot inspect {}: {error}",
                    entry.path().display()
                ))
            })?;
            if file_type.is_symlink() {
                return Err(CliError::new(format!(
                    "oracle artifact contains a symlink at {}",
                    entry.path().display()
                )));
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                total = total
                    .checked_add(
                        entry
                            .metadata()
                            .map_err(|error| {
                                CliError::new(format!(
                                    "cannot inspect {}: {error}",
                                    entry.path().display()
                                ))
                            })?
                            .len(),
                    )
                    .ok_or_else(|| CliError::new("oracle artifact size overflows u64"))?;
            }
        }
    }
    Ok(total)
}

fn estimate_oracle_bytes(layout: &ResolvedGradedProbeLayout) -> Result<OracleByteEstimate> {
    let probe_count = u64::try_from(layout.probes.len())
        .map_err(|_| CliError::new("oracle probe count exceeds u64"))?;
    let reachable_ordered_pair_count =
        reachable_ordered_probe_pair_count(&layout.probes, ORACLE_PATH_RANGE_M).map_err(
            |error| CliError::new(format!("cannot estimate oracle probe pairs: {error}")),
        )?;
    let estimated_raw_bytes = probe_count
        .checked_mul(PROBE_BYTES)
        .and_then(|bytes| {
            reachable_ordered_pair_count
                .checked_mul(REACHABLE_ORDERED_PAIR_BYTES)
                .and_then(|pairs| bytes.checked_add(pairs))
        })
        .and_then(|bytes| bytes.checked_add(SERIALIZATION_FIXED_BYTES))
        .ok_or_else(|| CliError::new("oracle byte estimate overflows u64"))?;
    let projected_low_bytes = estimated_raw_bytes
        .checked_mul(7)
        .map(|bytes| bytes / 10)
        .ok_or_else(|| CliError::new("oracle low byte estimate overflows u64"))?;
    let projected_high_bytes = estimated_raw_bytes
        .checked_mul(13)
        .and_then(|bytes| bytes.checked_add(9))
        .map(|bytes| bytes / 10)
        .ok_or_else(|| CliError::new("oracle high byte estimate overflows u64"))?;
    Ok(OracleByteEstimate {
        estimator_revision: "reachable-ordered-pairs-p256-q10-fixed64k-v1".to_owned(),
        path_horizon_m: ORACLE_PATH_RANGE_M,
        probe_count,
        reachable_ordered_pair_count,
        estimated_raw_bytes,
        projected_low_bytes,
        projected_high_bytes,
    })
}

fn verify_oracle_source_cells(
    route: &CityRouteManifest,
    inputs: &[CityRouteCellInput],
) -> Result<()> {
    if route.cells.len() != inputs.len() {
        return Err(CliError::new(
            "oracle cell arguments do not reproduce the supplied route cell count",
        ));
    }
    for (record, input) in route.cells.iter().zip(inputs) {
        let Some(completed) = record.city_bake.completed.as_ref() else {
            return Err(CliError::new(
                "oracle source route contains an incomplete cell bake",
            ));
        };
        let Some(input_bake) = input.completed_bake.as_ref() else {
            return Err(CliError::new(
                "oracle source cell argument lacks a completed bake",
            ));
        };
        if record.city_id != input.city_id
            || record.cell_id != input.cell_id
            || record.grid_index != input.grid_index
            || record.local_to_city_enu_m != input.local_to_city_enu_m
            || record.world.manifest_sha256 != input.world_manifest_sha256
            || record.world.mesh_sha256 != input.mesh_sha256
            || record.world.materials_sha256 != input.materials_sha256
            || record.city_bake.probe_plan_sidecar_sha256 != input.probe_plan_sidecar_sha256
            || record.city_bake.placement_policy_sha256 != input.probe_plan.placement_policy_sha256
            || record.city_bake.probe_layout_sha256 != input.probe_plan.probe_layout_sha256
            || record.city_bake.probe_count != input.probe_plan.byte_estimate.probe_count
            || completed.sidecar_sha256
                != input
                    .completed_bake_sidecar_sha256
                    .as_deref()
                    .unwrap_or_default()
            || completed.probe_batch_sha256 != input_bake.probe_batch.serialized_sha256
            || completed.probe_batch_size_bytes != input_bake.probe_batch.serialized_size_bytes
            || completed.path_data_size_bytes != input_bake.probe_batch.path_data_size_bytes
            || record.installed_size.package_bytes != input.installed_package_bytes
            || record.installed_size.baked_artifact_bytes != input.installed_bake_bytes
        {
            return Err(CliError::new(
                "cell package/bake arguments do not reproduce the supplied route identities",
            ));
        }
    }
    Ok(())
}

fn expand_bounds(bounds: CityBoundsMm, amount_mm: i64) -> Result<CityBoundsMm> {
    if amount_mm < 0 {
        return Err(CliError::new(
            "oracle geometry expansion must be non-negative",
        ));
    }
    Ok(CityBoundsMm::new(
        [
            bounds.min[0]
                .checked_sub(amount_mm)
                .ok_or_else(|| CliError::new("oracle geometry east minimum overflows"))?,
            bounds.min[1]
                .checked_sub(amount_mm)
                .ok_or_else(|| CliError::new("oracle geometry north minimum overflows"))?,
        ],
        [
            bounds.max[0]
                .checked_add(amount_mm)
                .ok_or_else(|| CliError::new("oracle geometry east maximum overflows"))?,
            bounds.max[1]
                .checked_add(amount_mm)
                .ok_or_else(|| CliError::new("oracle geometry north maximum overflows"))?,
        ],
    ))
}

fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| CliError::new(format!("cannot serialize oracle JSON: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|error| CliError::new(format!("cannot read {label} {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_world::{ElevatedProbeLayerPolicy, ProbeTierPolicy, SkyPathingPolicy};

    #[test]
    fn oracle_estimator_uses_its_1750m_horizon_without_mobile_limits() {
        let policy = GradedProbePolicy {
            lattice_origin_city_enu_mm: [0, 0],
            tiers: vec![ProbeTierPolicy {
                id: "residual".to_owned(),
                ground_up_mm: 1_500,
                ground_spacing_m: 32,
                analysis_spacing_m: 32,
                regions_city_enu_mm: vec![CityBoundsMm::new([-64_000, -64_000], [64_000, 64_000])],
                elevated_layers: vec![ElevatedProbeLayerPolicy {
                    id: "maximum".to_owned(),
                    up_mm: 63_000,
                    spacing_m: 32,
                }],
            }],
            sky_pathing_policy: SkyPathingPolicy::default(),
        };
        let layout = policy
            .resolve_bounds(CityBoundsMm::new([-64_000, -64_000], [64_000, 64_000]))
            .unwrap();
        let estimate = estimate_oracle_bytes(&layout).unwrap();
        assert_eq!(estimate.path_horizon_m, ORACLE_PATH_RANGE_M);
        assert_eq!(
            estimate.reachable_ordered_pair_count,
            estimate.probe_count * (estimate.probe_count - 1)
        );
        assert!(estimate.projected_high_bytes >= estimate.estimated_raw_bytes);
    }

    #[test]
    fn oracle_geometry_expansion_is_exact_and_checked() {
        let bounds = expand_bounds(
            CityBoundsMm::new([-342_500, -342_500], [827_500, 827_500]),
            600_000,
        )
        .unwrap();
        assert_eq!(bounds.min, [-942_500, -942_500]);
        assert_eq!(bounds.max, [1_427_500, 1_427_500]);
        assert!(expand_bounds(CityBoundsMm::new([i64::MIN + 1, 0], [1, 1]), 2).is_err());
    }
}
