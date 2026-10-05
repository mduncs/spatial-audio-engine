//! Placement-policy city bake using the exact probes emitted by
//! `fightbox.city-bake.v2`.

use std::path::Path;

use fightbox_evidence::sha256_hex;
use fightbox_steam_audio::{
    BakedProbeBatch, EXPLICIT_PROBE_BAKER_REVISION, ExplicitProbe, ExplicitProbeBakeRequest,
    PROBE_BATCH_METADATA_SCHEMA, PathBakeConfig, ProbeBatchMetadata, STEAM_AUDIO_UPSTREAM_COMMIT,
    STEAM_AUDIO_VERSION, bake_explicit_probe_batch,
};
use fightbox_world::{
    AboveMaximumLayerPolicy, CITY_BAKE_V2_CAPABILITY, CITY_BAKE_V2_SIDECAR_PATH,
    CityBakeV2BakedArtifact, CityBakeV2DeterministicTelemetry, CityBakeV2PathBakeSettings,
    CityBakeV2ProbeBatchIdentity, CityBakeV2ProbePlan, ExtensionRequirement,
    FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION, MOBILE_BAKED_PATH_HORIZON_M, PackageCompression,
    STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY, calibrated_probe_byte_model,
    read_package_with_capabilities,
};

use crate::atomicio::{
    AtomicDir, validate_output_path, write_bytes_atomic, write_json_string_atomic,
};
use crate::bake_reservation::{BakeReservation, format_bytes};
use crate::error::{CliError, Result};
use crate::probe_byte_estimate_v2::{
    PROBE_BYTE_ESTIMATE_V2_FILENAME, PROBE_BYTE_ESTIMATE_V2_SCHEMA,
    ProbeByteEstimateMeshObservableV2, ProbeByteEstimateModelV2, ProbeByteEstimateObservedV2,
    ProbeByteEstimatePairCountKindV2, ProbeByteEstimatePathingV2, ProbeByteEstimateRequestV2,
    ProbeByteEstimateSdkV2, ProbeByteEstimateSubjectV2, ProbeByteEstimateTierCountsV2,
    ProbeByteEstimateV2, request_sha256,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeByteModelSelection {
    Provisional,
    Wave17FixedTierMeshOpenPairsV2,
}

impl Default for ProbeByteModelSelection {
    fn default() -> Self {
        Self::Provisional
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BakeV2Config {
    pub visibility_samples: i32,
    pub probe_visibility_radius_m: f32,
    pub visibility_threshold: f32,
    pub visibility_range_m: f32,
    pub bake_threads: i32,
    pub probe_byte_model: ProbeByteModelSelection,
}

impl Default for BakeV2Config {
    fn default() -> Self {
        let defaults = PathBakeConfig::default();
        Self {
            visibility_samples: defaults.num_visibility_samples,
            probe_visibility_radius_m: defaults.probe_visibility_radius_m,
            visibility_threshold: defaults.visibility_threshold,
            visibility_range_m: defaults.visibility_range_m,
            bake_threads: defaults.num_threads,
            probe_byte_model: ProbeByteModelSelection::Provisional,
        }
    }
}

pub(crate) fn bake(package: &Path, output: &Path, config: BakeV2Config) -> Result<()> {
    let fixed = config.probe_byte_model == ProbeByteModelSelection::Wave17FixedTierMeshOpenPairsV2;
    bake_inner(package, output, config, fixed)
}

fn bake_inner(
    package: &Path,
    output: &Path,
    config: BakeV2Config,
    fixed_tier_mesh_model: bool,
) -> Result<()> {
    let loaded = read_package_with_capabilities(package, &[CITY_BAKE_V2_CAPABILITY])
        .map_err(|error| CliError::new(format!("cannot load v2 city package: {error}")))?;
    let world = loaded
        .manifest
        .world
        .as_ref()
        .ok_or_else(|| CliError::new("city bake-v2 requires a world-manifest-v2 package"))?;
    let plan_extension = loaded
        .manifest
        .extensions
        .iter()
        .find(|extension| extension.capability == CITY_BAKE_V2_CAPABILITY)
        .ok_or_else(|| {
            CliError::new(
                "city bake-v2 requires an indexed fightbox.city-bake.v2 probe-plan sidecar",
            )
        })?;
    if plan_extension.path != CITY_BAKE_V2_SIDECAR_PATH
        || plan_extension.requirement != ExtensionRequirement::Required
        || plan_extension.compression != PackageCompression::None
    {
        return Err(CliError::new(format!(
            "city bake-v2 plan must be an uncompressed required capability at {CITY_BAKE_V2_SIDECAR_PATH}"
        )));
    }
    let plan_path = package.join(CITY_BAKE_V2_SIDECAR_PATH);
    let plan_bytes = std::fs::read(&plan_path)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", plan_path.display())))?;
    let plan = CityBakeV2ProbePlan::from_sidecar_bytes(&plan_bytes)
        .map_err(|error| CliError::new(format!("invalid city-bake-v2 probe plan: {error}")))?;
    let canonical_plan_bytes = plan.to_sidecar_bytes().map_err(|error| {
        CliError::new(format!(
            "cannot canonicalize city-bake-v2 probe plan: {error}"
        ))
    })?;
    if plan_bytes != canonical_plan_bytes {
        return Err(CliError::new(
            "city bake-v2 probe plan is valid JSON but not the canonical planner serialization",
        ));
    }
    verify_plan_world_binding(&plan, world)?;
    if !plan.byte_estimate.projected_high_within_hard_limit {
        return Err(CliError::new(format!(
            "city bake-v2 projected high size {} exceeds the mobile hard limit; this lane does not silently override the cell contract",
            format_bytes(plan.byte_estimate.projected_high_bytes)
        )));
    }

    let probes = plan
        .local_probe_centres_m()
        .into_iter()
        .map(|(center, radius_m)| {
            ExplicitProbe::new(
                fightbox_steam_audio::EnuVector3::new(center[0], center[1], center[2]),
                radius_m,
            )
        })
        .collect::<Vec<_>>();
    let submitted_count = u64::try_from(probes.len())
        .map_err(|_| CliError::new("city bake-v2 probe count exceeds u64"))?;
    if submitted_count != plan.byte_estimate.probe_count {
        return Err(CliError::new(
            "city bake-v2 local-centre emission count differs from the validated plan",
        ));
    }
    let acoustic_mesh = loaded.mesh.clone();
    let scene_mesh = crate::city::scene_mesh(&loaded)?;
    let fixed_model = if fixed_tier_mesh_model {
        Some(precompute_fixed_model(
            &plan,
            &acoustic_mesh,
            &loaded.manifest.mesh_content_sha256,
            &config,
        )?)
    } else {
        None
    };

    let pathing = PathBakeConfig {
        num_visibility_samples: config.visibility_samples,
        probe_visibility_radius_m: config.probe_visibility_radius_m,
        visibility_threshold: config.visibility_threshold,
        visibility_range_m: config.visibility_range_m,
        path_range_m: MOBILE_BAKED_PATH_HORIZON_M as f32,
        num_threads: config.bake_threads,
    };
    let output = validate_output_path(output)?;
    let directory = AtomicDir::create(output.clone())?;
    let temp = directory.temp_path();
    let reservation_bytes = fixed_model
        .as_ref()
        .map_or(plan.byte_estimate.projected_high_bytes, |(model, _)| {
            model.projected_high_bytes
        });
    if let Some(model) = &fixed_model {
        if model.0.projected_high_bytes > fightbox_world::MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES {
            return Err(CliError::new(format!(
                "fixed tier/mesh projected high size {} exceeds the mobile hard limit; refusing admission",
                format_bytes(model.0.projected_high_bytes)
            )));
        }
    }
    let reservation =
        BakeReservation::create(&temp.join("probe-batch.bin"), &output, reservation_bytes)?;
    eprintln!(
        "fightbox: city bake-v2 reserved {} for {} exact explicit probes (filesystem reported {} available)",
        format_bytes(reservation_bytes),
        submitted_count,
        format_bytes(reservation.reported_available_bytes()),
    );

    // The package's probe-plan stays authoritative until this real SDK call and
    // validation both succeed. AtomicDir keeps partial output invisible.
    let baked = bake_explicit_probe_batch(&ExplicitProbeBakeRequest {
        mesh: scene_mesh.clone(),
        probes,
        pathing,
    })
    .map_err(|error| CliError::new(format!("explicit city probe bake failed: {error}")))?;
    baked
        .validate()
        .map_err(|error| CliError::new(format!("explicit city probe batch is invalid: {error}")))?;
    if u64::from(baked.metadata.probe_count) != submitted_count {
        return Err(CliError::new(
            "serialized city probe count differs from the exact submitted plan",
        ));
    }

    let mut completed = CityBakeV2BakedArtifact::bind_successful_bake(
        &plan,
        world.city.id.clone(),
        world.cell.id.clone(),
        loaded.manifest.mesh_content_sha256.clone(),
        loaded.manifest.materials_content_sha256.clone(),
        CityBakeV2PathBakeSettings {
            num_visibility_samples: pathing.num_visibility_samples,
            probe_visibility_radius_m: pathing.probe_visibility_radius_m,
            visibility_threshold: pathing.visibility_threshold,
            visibility_range_m: pathing.visibility_range_m,
            path_range_m: pathing.path_range_m,
            num_threads: pathing.num_threads,
        },
        CityBakeV2ProbeBatchIdentity {
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
        CityBakeV2DeterministicTelemetry {
            baker_revision: EXPLICIT_PROBE_BAKER_REVISION.to_owned(),
            submitted_explicit_probe_count: submitted_count,
            committed_probe_count: u64::from(baked.metadata.probe_count),
            insertion_order: "probe_plan_local_centres".to_owned(),
            bake_progress_callback_count: baked.metadata.bake_progress_callback_count,
            final_bake_progress_millionths: baked.metadata.final_bake_progress_millionths,
        },
    )
    .map_err(|error| {
        CliError::new(format!(
            "cannot bind completed city-bake-v2 sidecar: {error}"
        ))
    })?;
    if let Some((model, calibrated)) = &fixed_model {
        completed.estimator_revision = FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION.to_owned();
        completed.calibrated_model = Some(calibrated.clone());
        completed.estimated_raw_bytes = model.estimated_raw_bytes;
        completed.projected_low_bytes = model.projected_low_bytes;
        completed.projected_high_bytes = model.projected_high_bytes;
        completed.validate().map_err(|error| {
            CliError::new(format!(
                "cannot bind calibrated completed city-bake-v2: {error}"
            ))
        })?;
    }
    let completed_bytes = completed.to_sidecar_bytes().map_err(|error| {
        CliError::new(format!(
            "cannot serialize completed city-bake-v2 sidecar: {error}"
        ))
    })?;

    let metadata_json = baked.metadata.to_json();
    let actual_payload_bytes = u64::try_from(baked.bytes.len())
        .map_err(|_| CliError::new("city-bake-v2 probe batch exceeds u64"))?;
    let metadata_bytes = u64::try_from(metadata_json.len())
        .map_err(|_| CliError::new("city-bake-v2 metadata length exceeds u64"))?;
    let completed_sidecar_bytes = u64::try_from(completed_bytes.len())
        .map_err(|_| CliError::new("city-bake-v2 sidecar length exceeds u64"))?;
    let artifact_bytes_without_estimate = actual_payload_bytes
        .checked_add(metadata_bytes)
        .and_then(|bytes| bytes.checked_add(completed_sidecar_bytes))
        .ok_or_else(|| CliError::new("city-bake-v2 artifact byte count overflows u64"))?;
    let package_manifest_bytes = std::fs::read(package.join("manifest.json")).map_err(|error| {
        CliError::new(format!(
            "cannot read city-bake-v2 package manifest for estimator binding: {error}"
        ))
    })?;
    let mut estimate_request = ProbeByteEstimateRequestV2 {
        request_sha256: String::new(),
        mesh_sha256: loaded.manifest.mesh_content_sha256.clone(),
        materials_sha256: loaded.manifest.materials_content_sha256.clone(),
        probe_plan_sha256: Some(sha256_hex(&canonical_plan_bytes)),
        placement_policy_sha256: Some(plan.placement_policy_sha256.clone()),
        probe_layout_sha256: Some(plan.probe_layout_sha256.clone()),
        probe_count: submitted_count,
        path_horizon_m: plan.byte_estimate.path_horizon_m,
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
        CliError::new(format!("cannot identify probe estimate request: {error}"))
    })?;
    let estimate = ProbeByteEstimateV2 {
        schema_version: PROBE_BYTE_ESTIMATE_V2_SCHEMA.to_owned(),
        artifact_state: "completed".to_owned(),
        subject: ProbeByteEstimateSubjectV2 {
            schema_version: "fightbox.city-bake.v2".to_owned(),
            manifest_path: CITY_BAKE_V2_SIDECAR_PATH.to_owned(),
            manifest_sha256: sha256_hex(&completed_bytes),
            package_manifest_sha256: sha256_hex(&package_manifest_bytes),
        },
        request: estimate_request,
        model: fixed_model
            .as_ref()
            .map(|(model, _)| model.clone())
            .unwrap_or_else(|| ProbeByteEstimateModelV2 {
                revision: plan.byte_estimate.estimator_revision.clone(),
                probe_bytes: 256,
                pair_bytes: 10,
                fixed_bytes: 64 * 1_024,
                pair_count: plan.byte_estimate.reachable_ordered_pair_count,
                pair_count_kind: ProbeByteEstimatePairCountKindV2::Exact,
                estimated_raw_bytes: plan.byte_estimate.estimated_raw_bytes,
                projected_low_bytes: plan.byte_estimate.projected_low_bytes,
                projected_high_bytes: plan.byte_estimate.projected_high_bytes,
                reservation_bytes: reservation.reserved_bytes(),
                calibrated_model: None,
            }),
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
            CliError::new(format!("cannot finalize probe-byte estimate v2: {error}"))
        })?;
    let additional_artifact_bytes = estimate
        .observed
        .artifact_bytes
        .checked_sub(actual_payload_bytes)
        .ok_or_else(|| CliError::new("probe estimate artifact bytes omit payload"))?;
    reservation.assert_post_bake(
        actual_payload_bytes,
        baked.metadata.serialized_size_bytes,
        additional_artifact_bytes,
    )?;
    reservation.finish(&baked.bytes)?;
    write_json_string_atomic(&temp.join("probe-batch-metadata.json"), &metadata_json)?;
    let capability_directory = temp.join("capabilities");
    std::fs::create_dir_all(&capability_directory).map_err(|error| {
        CliError::new(format!(
            "cannot create completed city-bake-v2 capability directory: {error}"
        ))
    })?;
    write_bytes_atomic(&temp.join(CITY_BAKE_V2_SIDECAR_PATH), &completed_bytes)?;
    write_bytes_atomic(&temp.join(PROBE_BYTE_ESTIMATE_V2_FILENAME), &estimate_bytes)?;
    verify_probe_byte_estimate(temp, package)?;
    directory.commit()?;
    verify_probe_byte_estimate(&output, package)?;
    eprintln!(
        "fightbox: city bake-v2 written to {} (city={}, cell={}, probes={}, serialized={}, sha256={})",
        output.display(),
        completed.city_id,
        completed.cell_id,
        completed.probe_batch.probe_count,
        format_bytes(completed.probe_batch.serialized_size_bytes),
        completed.probe_batch.serialized_sha256,
    );
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeBatchMetadataSidecarV1 {
    schema_version: String,
    steam_audio_version: String,
    upstream_commit: String,
    probe_count: u64,
    path_data_size_bytes: u64,
    serialized_size_bytes: u64,
    content_sha256: String,
    bake_progress_callback_count: u32,
    final_bake_progress_millionths: u32,
}

fn precompute_fixed_model(
    plan: &CityBakeV2ProbePlan,
    mesh: &fightbox_world::AcousticMesh,
    mesh_sha256: &str,
    config: &BakeV2Config,
) -> Result<(
    ProbeByteEstimateModelV2,
    fightbox_world::CalibratedProbeByteModelV2,
)> {
    let tier_count = |id: &str| -> Result<u64> {
        plan.tier_summaries
            .iter()
            .find(|tier| tier.id == id)
            .map(|tier| tier.probe_count)
            .ok_or_else(|| CliError::new(format!("fixed estimator requires calibrated {id} tier")))
    };
    let tier_counts = ProbeByteEstimateTierCountsV2 {
        owner_home_count: tier_count("owner-home")?,
        route_core_count: tier_count("route-core")?,
        transition_count: tier_count("transition")?,
        residual_count: tier_count("residual")?,
    };
    let mut calibrated = calibrated_probe_byte_model(plan, mesh)
        .map_err(|error| CliError::new(format!("cannot compute fixed mesh observable: {error}")))?;
    calibrated.mesh_observable.mesh_sha256 = Some(mesh_sha256.to_owned());
    calibrated
        .validate()
        .map_err(|error| CliError::new(format!("cannot bind fixed mesh hash: {error}")))?;
    let observable = ProbeByteEstimateMeshObservableV2 {
        algorithm: calibrated.mesh_observable.algorithm.clone(),
        coordinate_encoding: calibrated.mesh_observable.coordinate_encoding.clone(),
        mesh_sha256: mesh_sha256.to_owned(),
        owner_ground_height_mm: calibrated.mesh_observable.owner_ground_height_mm,
        owner_ground_probe_count: calibrated.mesh_observable.owner_ground_probe_count,
        wall_edge_count: calibrated.mesh_observable.wall_edge_count,
        blocked_owner_ground_ordered_pair_count: calibrated
            .mesh_observable
            .blocked_owner_ground_ordered_pair_count,
        open_owner_ground_ordered_pair_count: calibrated
            .mesh_observable
            .open_owner_ground_ordered_pair_count,
    };
    // Request fields affecting path serialization are fixed before compute;
    // this is safe to use for reservation and is rebuilt identically below.
    let mut request = ProbeByteEstimateRequestV2 {
        request_sha256: String::new(),
        mesh_sha256: mesh_sha256.to_owned(),
        materials_sha256: "0".repeat(64),
        probe_plan_sha256: None,
        placement_policy_sha256: Some(plan.placement_policy_sha256.clone()),
        probe_layout_sha256: Some(plan.probe_layout_sha256.clone()),
        probe_count: plan.byte_estimate.probe_count,
        path_horizon_m: plan.byte_estimate.path_horizon_m,
        pathing: ProbeByteEstimatePathingV2 {
            visibility_range_m: config.visibility_range_m,
            visibility_samples: config.visibility_samples,
            visibility_threshold: config.visibility_threshold,
            probe_visibility_radius_m: config.probe_visibility_radius_m,
            threads: config.bake_threads,
        },
        sdk: ProbeByteEstimateSdkV2 {
            metadata_schema: PROBE_BATCH_METADATA_SCHEMA.to_owned(),
            steam_audio_version: STEAM_AUDIO_VERSION.to_owned(),
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT.to_owned(),
            baker_revision: EXPLICIT_PROBE_BAKER_REVISION.to_owned(),
        },
    };
    request.request_sha256 = request_sha256(&request).map_err(|error| {
        CliError::new(format!("cannot identify fixed estimator request: {error}"))
    })?;
    let model = ProbeByteEstimateV2::fixed_tier_mesh_model(&request, tier_counts, observable)
        .map_err(|error| CliError::new(format!("cannot compute fixed estimator model: {error}")))?;
    Ok((model, calibrated))
}

pub(crate) struct PreverifiedProbeBatch {
    /// Exact bytes previously read from `artifact.join("probe-batch.bin")`
    /// earlier in the same invocation.  Callers must only supply bytes read
    /// from that exact path; substituting them for the verify-time read of the
    /// same immutable file is byte-identical.
    pub bytes: Vec<u8>,
    /// SHA-256 of [`PreverifiedProbeBatch::bytes`], hex-encoded.
    pub sha256_hex: String,
}

pub(crate) fn verify_probe_byte_estimate(artifact: &Path, package: &Path) -> Result<()> {
    verify_probe_byte_estimate_preverified(artifact, package, None)
}

pub(crate) fn verify_probe_byte_estimate_preverified(
    artifact: &Path,
    package: &Path,
    preverified_batch: Option<PreverifiedProbeBatch>,
) -> Result<()> {
    validate_city_bake_artifact_layout(artifact, true)?;
    let estimate_path = artifact.join(PROBE_BYTE_ESTIMATE_V2_FILENAME);
    let estimate_bytes = std::fs::read(&estimate_path).map_err(|error| {
        CliError::new(format!("cannot read {}: {error}", estimate_path.display()))
    })?;
    let estimate = ProbeByteEstimateV2::from_json(&estimate_bytes)
        .map_err(|error| CliError::new(format!("invalid probe-byte estimate v2: {error}")))?;
    if estimate.to_canonical_json().map_err(|error| {
        CliError::new(format!(
            "cannot canonicalize probe-byte estimate v2: {error}"
        ))
    })? != estimate_bytes
    {
        return Err(CliError::new(
            "probe-byte estimate v2 is valid JSON but not canonical bytes",
        ));
    }

    let completed_path = artifact.join(CITY_BAKE_V2_SIDECAR_PATH);
    let completed_bytes = std::fs::read(&completed_path).map_err(|error| {
        CliError::new(format!("cannot read {}: {error}", completed_path.display()))
    })?;
    let completed = CityBakeV2BakedArtifact::from_sidecar_bytes(&completed_bytes)
        .map_err(|error| CliError::new(format!("invalid completed city-bake-v2: {error}")))?;
    if completed.to_sidecar_bytes().map_err(|error| {
        CliError::new(format!(
            "cannot canonicalize completed city-bake-v2: {error}"
        ))
    })? != completed_bytes
    {
        return Err(CliError::new(
            "completed city-bake-v2 is valid JSON but not canonical bytes",
        ));
    }

    let loaded = read_package_with_capabilities(package, &[CITY_BAKE_V2_CAPABILITY])
        .map_err(|error| CliError::new(format!("cannot load estimator package: {error}")))?;
    let world = loaded
        .manifest
        .world
        .as_ref()
        .ok_or_else(|| CliError::new("estimator package is not world-manifest-v2"))?;
    let plan_extension = loaded
        .manifest
        .extensions
        .iter()
        .find(|extension| extension.capability == CITY_BAKE_V2_CAPABILITY)
        .ok_or_else(|| CliError::new("estimator package does not index city-bake-v2"))?;
    if plan_extension.path != CITY_BAKE_V2_SIDECAR_PATH
        || plan_extension.requirement != ExtensionRequirement::Required
        || plan_extension.compression != PackageCompression::None
    {
        return Err(CliError::new(format!(
            "estimator plan must be an uncompressed required capability at {CITY_BAKE_V2_SIDECAR_PATH}"
        )));
    }
    let package_manifest_bytes = std::fs::read(package.join("manifest.json")).map_err(|error| {
        CliError::new(format!("cannot read estimator package manifest: {error}"))
    })?;
    let plan_bytes = std::fs::read(package.join(CITY_BAKE_V2_SIDECAR_PATH))
        .map_err(|error| CliError::new(format!("cannot read estimator probe plan: {error}")))?;
    let plan = CityBakeV2ProbePlan::from_sidecar_bytes(&plan_bytes)
        .map_err(|error| CliError::new(format!("invalid estimator probe plan: {error}")))?;
    let canonical_plan_bytes = plan.to_sidecar_bytes().map_err(|error| {
        CliError::new(format!("cannot canonicalize estimator probe plan: {error}"))
    })?;
    if canonical_plan_bytes != plan_bytes {
        return Err(CliError::new(
            "estimator probe plan is valid JSON but not canonical bytes",
        ));
    }
    verify_plan_world_binding(&plan, world)?;
    if !plan.byte_estimate.projected_high_within_hard_limit
        || plan.byte_estimate.hard_raw_probe_payload_bytes != 67_108_864
        || plan.byte_estimate.target_raw_probe_payload_bytes != 50_331_648
        || plan.byte_estimate.path_horizon_m != MOBILE_BAKED_PATH_HORIZON_M as u32
    {
        return Err(CliError::new(
            "estimator city-bake plan is not mobile-admissible; use the desktop oracle lane for over-hard calibration",
        ));
    }
    // Recompute the package/plan-only calibrated data at verification time.
    // A completed-sidecar copy is never trusted merely because it validates.
    // The plan/mesh recomputation runs exactly once per verify call; every
    // later comparison (completed-model identity, calibrated-domain admission,
    // and the fixed-tier observable) derives from this single result without
    // changing any verdict.
    let recalibrated = fightbox_world::calibrated_probe_byte_model(&plan, &loaded.mesh);
    let recomputed_calibrated_model = recalibrated.as_ref().ok().and_then(|model| {
        let mut model = model.clone();
        model.mesh_observable.mesh_sha256 = Some(loaded.manifest.mesh_content_sha256.clone());
        model.validate().ok().map(|()| model)
    });

    let (payload, payload_sha256) = match preverified_batch {
        Some(preverified) => (preverified.bytes, preverified.sha256_hex),
        None => {
            let payload = std::fs::read(artifact.join("probe-batch.bin")).map_err(|error| {
                CliError::new(format!("cannot read estimator probe batch: {error}"))
            })?;
            let payload_sha256 = sha256_hex(&payload);
            (payload, payload_sha256)
        }
    };
    let metadata_bytes = std::fs::read(artifact.join("probe-batch-metadata.json"))
        .map_err(|error| CliError::new(format!("cannot read estimator metadata: {error}")))?;
    let metadata: ProbeBatchMetadataSidecarV1 = serde_json::from_slice(&metadata_bytes)
        .map_err(|error| CliError::new(format!("invalid estimator metadata: {error}")))?;
    let metadata_probe_count = u32::try_from(metadata.probe_count)
        .map_err(|_| CliError::new("estimator metadata probe count exceeds u32"))?;
    let canonical_metadata = ProbeBatchMetadata {
        schema_version: PROBE_BATCH_METADATA_SCHEMA,
        steam_audio_version: STEAM_AUDIO_VERSION,
        upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
        probe_count: metadata_probe_count,
        path_data_size_bytes: metadata.path_data_size_bytes,
        serialized_size_bytes: metadata.serialized_size_bytes,
        content_sha256: metadata.content_sha256.clone(),
        bake_progress_callback_count: metadata.bake_progress_callback_count,
        final_bake_progress_millionths: metadata.final_bake_progress_millionths,
    };
    if canonical_metadata.to_json().as_bytes() != metadata_bytes {
        return Err(CliError::new(
            "probe-batch metadata is valid JSON but not canonical bytes",
        ));
    }
    let installed_bytes = directory_file_bytes(artifact)?;
    let plan_sha256 = sha256_hex(&canonical_plan_bytes);
    let completed_sha256 = sha256_hex(&completed_bytes);

    let identities_match = estimate.subject.schema_version == "fightbox.city-bake.v2"
        && estimate.subject.manifest_path == CITY_BAKE_V2_SIDECAR_PATH
        && estimate.subject.manifest_sha256 == completed_sha256
        && estimate.subject.package_manifest_sha256 == sha256_hex(&package_manifest_bytes)
        && estimate.request.mesh_sha256 == loaded.manifest.mesh_content_sha256
        && estimate.request.materials_sha256 == loaded.manifest.materials_content_sha256
        && estimate.request.probe_plan_sha256.as_deref() == Some(plan_sha256.as_str())
        && estimate.request.placement_policy_sha256.as_deref()
            == Some(plan.placement_policy_sha256.as_str())
        && estimate.request.probe_layout_sha256.as_deref()
            == Some(plan.probe_layout_sha256.as_str())
        && completed.coordinate_frame == plan.coordinate_frame
        && completed.coordinate_encoding == plan.coordinate_encoding
        && completed.placement_rule == plan.placement_rule
        && completed.probe_plan_content_sha256 == plan_sha256
        && completed.placement_policy_sha256 == plan.placement_policy_sha256
        && completed.probe_layout_sha256 == plan.probe_layout_sha256
        && completed.estimator_revision == estimate.model.revision
        && match estimate.model.revision.as_str() {
            FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION => {
                completed.calibrated_model.as_ref() == recomputed_calibrated_model.as_ref()
            }
            crate::probe_byte_estimate_v2::PROVISIONAL_ESTIMATOR_REVISION => {
                completed.calibrated_model.is_none()
            }
            _ => false,
        }
        && completed.city_id == world.city.id
        && completed.cell_id == world.cell.id
        && completed.cell_grid_index == plan.cell.grid_index
        && completed.tier_summaries == plan.tier_summaries
        && completed.sky_pathing_policy == plan.policy.sky_pathing_policy
        && completed.planned_probe_count == plan.byte_estimate.probe_count
        && completed.estimated_raw_bytes == estimate.model.estimated_raw_bytes
        && completed.projected_low_bytes == estimate.model.projected_low_bytes
        && completed.projected_high_bytes == estimate.model.projected_high_bytes
        && completed.package_mesh_sha256 == estimate.request.mesh_sha256
        && completed.package_materials_sha256 == estimate.request.materials_sha256;
    if !identities_match {
        return Err(CliError::new(
            "probe-byte estimate request/subject differs from its world, package, plan, or completed bake",
        ));
    }

    let pathing_matches = estimate.request.path_horizon_m == MOBILE_BAKED_PATH_HORIZON_M as u32
        && completed.pathing.path_range_m.to_bits()
            == (MOBILE_BAKED_PATH_HORIZON_M as f32).to_bits()
        && estimate.request.pathing.visibility_range_m.to_bits()
            == completed.pathing.visibility_range_m.to_bits()
        && estimate.request.pathing.visibility_samples == completed.pathing.num_visibility_samples
        && estimate.request.pathing.visibility_threshold.to_bits()
            == completed.pathing.visibility_threshold.to_bits()
        && estimate.request.pathing.probe_visibility_radius_m.to_bits()
            == completed.pathing.probe_visibility_radius_m.to_bits()
        && estimate.request.pathing.threads == completed.pathing.num_threads;
    if !pathing_matches {
        return Err(CliError::new(
            "probe-byte estimate pathing differs from the completed mobile bake",
        ));
    }

    let completed_model_matches =
        if estimate.model.revision == FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION {
            let expected = match recalibrated.as_ref() {
                Ok(model) => {
                    let mut model = model.clone();
                    model.mesh_observable.mesh_sha256 = Some(estimate.request.mesh_sha256.clone());
                    model
                }
                Err(error) => {
                    return Err(CliError::new(format!(
                        "cannot recompute completed calibrated model: {error}"
                    )));
                }
            };
            completed.calibrated_model.as_ref() == Some(&expected)
        } else {
            completed.calibrated_model.is_none()
        };
    let model_matches = completed_model_matches
        && if estimate.model.revision == plan.byte_estimate.estimator_revision {
            estimate.request.probe_count == plan.byte_estimate.probe_count
                && estimate.model.pair_count == plan.byte_estimate.reachable_ordered_pair_count
                && estimate.model.pair_count_kind == ProbeByteEstimatePairCountKindV2::Exact
                && estimate.model.estimated_raw_bytes == plan.byte_estimate.estimated_raw_bytes
                && estimate.model.projected_low_bytes == plan.byte_estimate.projected_low_bytes
                && estimate.model.projected_high_bytes == plan.byte_estimate.projected_high_bytes
                && estimate.model.reservation_bytes == plan.byte_estimate.projected_high_bytes
        } else if estimate.model.revision == FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION {
            // Domain admission is derived from the single canonical plan/mesh
            // recomputation above, not inferred from the envelope's tier-count
            // fields.  This rejects a tampered plan policy, tier shape, or owner
            // layer before comparing arithmetic.
            let calibrated_domain = recalibrated.is_ok();
            // When the calibrated model was admitted, its embedded observable
            // is field-for-field the pure `mesh_open_pair_observable` result for
            // this exact plan/mesh pair (the pure helper leaves `mesh_sha256`
            // absent and the builder embeds it verbatim), so deriving it here is
            // identical to a second traversal.  When admission failed, the
            // conjunction below is false regardless of the observable.
            let observable =
                recalibrated
                    .as_ref()
                    .ok()
                    .map(|model| ProbeByteEstimateMeshObservableV2 {
                        algorithm: model.mesh_observable.algorithm.clone(),
                        coordinate_encoding: model.mesh_observable.coordinate_encoding.clone(),
                        mesh_sha256: estimate.request.mesh_sha256.clone(),
                        owner_ground_height_mm: model.mesh_observable.owner_ground_height_mm,
                        owner_ground_probe_count: model.mesh_observable.owner_ground_probe_count,
                        wall_edge_count: model.mesh_observable.wall_edge_count,
                        blocked_owner_ground_ordered_pair_count: model
                            .mesh_observable
                            .blocked_owner_ground_ordered_pair_count,
                        open_owner_ground_ordered_pair_count: model
                            .mesh_observable
                            .open_owner_ground_ordered_pair_count,
                    });
            let tier_count = |id: &str| {
                plan.tier_summaries
                    .iter()
                    .find(|tier| tier.id == id)
                    .map(|tier| tier.probe_count)
            };
            calibrated_domain
                && observable.and_then(|observable| {
                    ProbeByteEstimateV2::fixed_tier_mesh_model(
                        &estimate.request,
                        ProbeByteEstimateTierCountsV2 {
                            owner_home_count: tier_count("owner-home")?,
                            route_core_count: tier_count("route-core")?,
                            transition_count: tier_count("transition")?,
                            residual_count: tier_count("residual")?,
                        },
                        observable,
                    )
                    .ok()
                }) == Some(estimate.model.clone())
        } else {
            false
        };
    if !model_matches {
        return Err(CliError::new(
            "probe-byte estimate model or reservation differs from the canonical mobile plan",
        ));
    }

    let sdk_matches = estimate.request.sdk.metadata_schema == PROBE_BATCH_METADATA_SCHEMA
        && estimate.request.sdk.steam_audio_version == STEAM_AUDIO_VERSION
        && estimate.request.sdk.upstream_commit == STEAM_AUDIO_UPSTREAM_COMMIT
        && estimate.request.sdk.baker_revision == EXPLICIT_PROBE_BAKER_REVISION
        && completed.probe_batch.capability == STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY
        && completed.probe_batch.payload_path == "probe-batch.bin"
        && completed.probe_batch.compression == PackageCompression::None
        && completed.probe_batch.metadata_schema == PROBE_BATCH_METADATA_SCHEMA
        && completed.probe_batch.steam_audio_version == STEAM_AUDIO_VERSION
        && completed.probe_batch.upstream_commit == STEAM_AUDIO_UPSTREAM_COMMIT
        && completed.telemetry.baker_revision == EXPLICIT_PROBE_BAKER_REVISION
        && completed.telemetry.submitted_explicit_probe_count == plan.byte_estimate.probe_count
        && completed.telemetry.committed_probe_count == plan.byte_estimate.probe_count
        && completed.telemetry.insertion_order == "probe_plan_local_centres";
    if !sdk_matches {
        return Err(CliError::new(
            "probe-byte estimate SDK or probe-batch authority differs from the production contract",
        ));
    }

    let metadata_matches = metadata.schema_version == PROBE_BATCH_METADATA_SCHEMA
        && metadata.steam_audio_version == STEAM_AUDIO_VERSION
        && metadata.upstream_commit == STEAM_AUDIO_UPSTREAM_COMMIT
        && metadata.probe_count == estimate.observed.probe_count
        && metadata.path_data_size_bytes == estimate.observed.path_data_size_bytes
        && metadata.serialized_size_bytes == estimate.observed.serialized_size_bytes
        && metadata.content_sha256 == estimate.observed.payload_sha256
        && metadata.bake_progress_callback_count
            == completed.telemetry.bake_progress_callback_count
        && metadata.final_bake_progress_millionths
            == completed.telemetry.final_bake_progress_millionths;
    if !metadata_matches {
        return Err(CliError::new(
            "probe-byte estimate metadata differs from the SDK, observations, or completed bake",
        ));
    }

    let payload_len = u64::try_from(payload.len())
        .map_err(|_| CliError::new("probe-byte estimate payload length exceeds u64"))?;
    if estimate.observed.payload_sha256 != payload_sha256
        || estimate.observed.serialized_size_bytes != payload_len
        || estimate.observed.serialized_size_bytes != completed.probe_batch.serialized_size_bytes
        || estimate.observed.path_data_size_bytes != completed.probe_batch.path_data_size_bytes
        || estimate.observed.probe_count != completed.probe_batch.probe_count
        || completed.probe_batch.serialized_sha256 != payload_sha256
        || estimate.observed.artifact_bytes != installed_bytes
    {
        return Err(CliError::new(
            "probe-byte estimate observations differ from payload, completed bake, or installed bytes",
        ));
    }

    let batch = BakedProbeBatch {
        metadata: canonical_metadata,
        bytes: payload,
    };
    batch
        .validate()
        .map_err(|error| CliError::new(format!("estimator probe batch is invalid: {error}")))?;
    let coverage = batch.probe_coverage().map_err(|error| {
        CliError::new(format!("cannot decode estimator probe coverage: {error}"))
    })?;
    let planned = plan.local_probe_centres_m();
    if coverage.probe_count() != planned.len()
        || !coverage.spheres().zip(planned.iter()).all(
            |((center, radius), (expected, expected_radius))| {
                center.x.to_bits() == expected[0].to_bits()
                    && center.y.to_bits() == expected[1].to_bits()
                    && center.z.to_bits() == expected[2].to_bits()
                    && radius.to_bits() == expected_radius.to_bits()
            },
        )
    {
        return Err(CliError::new(
            "serialized probe spheres differ from the exact ordered plan",
        ));
    }

    eprintln!(
        "fightbox: probe-byte estimate v2 verified at {} (probes={}, pairs={}, payload={}, artifact={})",
        estimate_path.display(),
        estimate.request.probe_count,
        estimate.model.pair_count,
        format_bytes(estimate.observed.serialized_size_bytes),
        format_bytes(estimate.observed.artifact_bytes),
    );
    Ok(())
}

pub(crate) fn validate_city_bake_artifact_layout(
    root: &Path,
    require_estimate: bool,
) -> Result<()> {
    let mut root_entries = std::fs::read_dir(root)
        .map_err(|error| CliError::new(format!("cannot inspect {}: {error}", root.display())))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| CliError::new(format!("cannot inspect city-bake artifact: {error}")))?;
    root_entries.sort();
    let mut expected = vec![
        std::ffi::OsString::from("capabilities"),
        std::ffi::OsString::from("probe-batch-metadata.json"),
        std::ffi::OsString::from("probe-batch.bin"),
    ];
    if require_estimate {
        expected.push(std::ffi::OsString::from(PROBE_BYTE_ESTIMATE_V2_FILENAME));
    }
    expected.sort();
    if root_entries != expected {
        return Err(CliError::new(
            "city-bake-v2 artifact must contain exactly payload, metadata, completed authority, and estimator envelope",
        ));
    }
    let capabilities = root.join("capabilities");
    let entries = std::fs::read_dir(&capabilities)
        .map_err(|error| {
            CliError::new(format!(
                "cannot inspect {}: {error}",
                capabilities.display()
            ))
        })?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| {
            CliError::new(format!("cannot inspect completed capabilities: {error}"))
        })?;
    if entries.len() != 1
        || entries[0].file_name() != std::ffi::OsStr::new("city-bake-v2.json")
        || !entries[0]
            .file_type()
            .map_err(|error| {
                CliError::new(format!("cannot inspect completed capability: {error}"))
            })?
            .is_file()
    {
        return Err(CliError::new(
            "city-bake-v2 capabilities directory must contain only city-bake-v2.json",
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
            let entry = entry
                .map_err(|error| CliError::new(format!("cannot inspect artifact: {error}")))?;
            let file_type = entry.file_type().map_err(|error| {
                CliError::new(format!(
                    "cannot inspect {}: {error}",
                    entry.path().display()
                ))
            })?;
            if file_type.is_symlink() {
                return Err(CliError::new(format!(
                    "probe-byte estimate artifact contains a symlink at {}",
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
                    .ok_or_else(|| {
                        CliError::new("probe-byte estimate artifact size overflows u64")
                    })?;
            }
        }
    }
    Ok(total)
}

fn verify_plan_world_binding(
    plan: &CityBakeV2ProbePlan,
    world: &fightbox_world::WorldPackageV2Index,
) -> Result<()> {
    if plan.cell.grid_index != world.cell.grid_index {
        return Err(CliError::new(
            "city-bake-v2 probe plan grid index differs from the package cell",
        ));
    }
    let translation = plan
        .cell
        .local_to_city_enu_mm
        .map(|millimetres| millimetres as f64 / 1_000.0);
    if translation != world.cell.local_to_city_enu_m {
        return Err(CliError::new(
            "city-bake-v2 probe plan local origin differs from the package cell transform",
        ));
    }
    if plan.policy.sky_pathing_policy.maximum_layer_m != 63
        || plan.policy.sky_pathing_policy.above != AboveMaximumLayerPolicy::DirectReflectionsOnly
    {
        return Err(CliError::new(
            "city-bake-v2 plan must declare direct+reflections-only above 63 m",
        ));
    }
    Ok(())
}
