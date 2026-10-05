//! Deterministic assembly of streamed city-cell routes.
//!
//! This module owns package identity and adjacency metadata only. It does not
//! bake probes, load Steam Audio, or prepare runtime worlds.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    AboveMaximumLayerPolicy, CELL_GEOMETRY_HALO_M, CELL_OWNERSHIP_GUARD_M, CELL_PAIRWISE_OVERLAP_M,
    CELL_PROBE_FOOTPRINT_M, CELL_STRIDE_M, CalibratedProbeByteModelV2, CellGridIndex,
    CellProbeSlice, CityBakeV2BakedArtifact, CityBakeV2ProbePlan, CityBoundsMm,
    ECHO_AUTHORITY_CAPABILITY, ECHO_AUTHORITY_SIDECAR_PATH, MOBILE_ECHO_AUTHORITY_CAP_BYTES,
    ProbeSite, Result, WorldError, sha256::sha256_hex, stable_cell_id,
};

pub const CITY_ROUTE_SCHEMA_ID: &str = "fightbox.city-route.v1";
pub const CITY_ROUTE_MANIFEST_FILENAME: &str = "city-route-manifest.json";
pub const CITY_ROUTE_ASSEMBLER_REVISION: &str = "global-slice-route-assembly-v1";

const MILLIMETRES_PER_METRE: i64 = 1_000;
const PROBE_RESIDENCY_MULTIPLIER_NUMERATOR: u64 = 329;
const PROBE_RESIDENCY_MULTIPLIER_DENOMINATOR: u64 = 100;
const FOUR_CELL_ORACLE_MARGIN_MM: i64 = 50_000;
const FOUR_CELL_ORACLE_PATH_RANGE_M: u32 = 1_750;

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, PartialEq)]
pub struct CityRouteCellInput {
    pub city_id: String,
    pub cell_id: String,
    pub grid_index: CellGridIndex,
    pub local_to_city_enu_m: [f64; 3],
    pub world_manifest_sha256: String,
    pub mesh_sha256: String,
    pub materials_sha256: String,
    pub probe_plan_sidecar_sha256: String,
    pub probe_plan: CityBakeV2ProbePlan,
    pub completed_bake_sidecar_sha256: Option<String>,
    pub completed_bake: Option<CityBakeV2BakedArtifact>,
    pub probe_byte_estimate_v2_sha256: Option<String>,
    pub probe_byte_estimate_v2_size_bytes: Option<u64>,
    pub echo_authority: Option<CityRouteEchoAuthorityInput>,
    pub installed_package_bytes: u64,
    pub installed_bake_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CityRouteEchoAuthorityInput {
    pub content_sha256: String,
    pub serialized_size_bytes: u64,
    pub resident_size_bytes: u64,
    pub anchor_set_sha256: String,
    pub listener_layout_sha256: String,
    pub coordinate_frame_key: String,
    pub static_anchor_count: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CityRouteAssemblyRequest {
    pub route_id: String,
    /// Authored route order. Consecutive cells must be grid-adjacent.
    pub cells: Vec<CityRouteCellInput>,
    pub owner_home_cell_id: Option<String>,
    pub include_four_cell_fixture: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CityRouteBakeState {
    ProbePlan,
    BakedProbeBatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteGridPolicy {
    pub probe_footprint_m: u32,
    pub stride_m: u32,
    pub pairwise_overlap_m: u32,
    pub ownership_guard_m: u32,
    pub geometry_halo_m: u32,
    pub maximum_resident_worlds: u32,
    pub maximum_prepared_neighbors: u32,
    pub maximum_resident_echo_authority_cells: u32,
    pub echo_authority_resident_cap_bytes: u64,
}

impl Default for CityRouteGridPolicy {
    fn default() -> Self {
        Self {
            probe_footprint_m: 585,
            stride_m: 485,
            pairwise_overlap_m: 100,
            ownership_guard_m: 50,
            geometry_halo_m: 600,
            maximum_resident_worlds: 2,
            maximum_prepared_neighbors: 1,
            maximum_resident_echo_authority_cells: 2,
            echo_authority_resident_cap_bytes: MOBILE_ECHO_AUTHORITY_CAP_BYTES as u64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteWorldBinding {
    pub manifest_sha256: String,
    pub mesh_sha256: String,
    pub materials_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteCompletedBakeBinding {
    pub sidecar_sha256: String,
    pub probe_batch_sha256: String,
    pub probe_batch_size_bytes: u64,
    pub path_data_size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_byte_estimate_v2_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_byte_estimate_v2_size_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteBakeBinding {
    pub state: CityRouteBakeState,
    pub probe_plan_sidecar_sha256: String,
    pub placement_policy_sha256: String,
    pub probe_layout_sha256: String,
    pub probe_count: u64,
    pub tier_ids: Vec<String>,
    pub maximum_layer_m: u32,
    pub above_maximum_layer: AboveMaximumLayerPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibrated_model: Option<CalibratedProbeByteModelV2>,
    pub completed: Option<CityRouteCompletedBakeBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteEchoAuthorityBinding {
    pub capability: String,
    pub package_sidecar_path: String,
    pub content_sha256: String,
    pub serialized_size_bytes: u64,
    pub resident_size_bytes: u64,
    pub anchor_set_sha256: String,
    pub listener_layout_sha256: String,
    pub coordinate_frame_key: String,
    pub static_anchor_count: u32,
    pub residency_scope: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteSelectionMetadata {
    pub route_offset_mm: i64,
    pub ownership_bounds_city_enu_mm: CityBoundsMm,
    pub guard_bounds_city_enu_mm: CityBoundsMm,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRoutePrefetchMetadata {
    pub reverse_cell_id: Option<String>,
    pub forward_cell_id: Option<String>,
    /// Direct input to `CellPrepareEstimate::raw_cell_bytes`.
    pub raw_cell_bytes: u64,
    /// Direct input to `CellPrepareEstimate::prepared_resident_bytes`.
    pub prepared_resident_estimate_bytes: u64,
    /// Direct input to `CellPrepareEstimate::preparation_scratch_bytes`.
    pub preparation_scratch_bytes: u64,
    pub echo_authority_resident_bytes: u64,
    pub echo_authority_load_policy: String,
    pub estimate_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteInstalledSize {
    pub package_bytes: u64,
    pub baked_artifact_bytes: u64,
    pub actual_installed_bytes: u64,
    pub projected_remaining_bake_bytes: u64,
    pub projected_complete_installed_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteCellRecord {
    pub sequence: u32,
    pub city_id: String,
    pub cell_id: String,
    pub grid_index: CellGridIndex,
    pub local_to_city_enu_m: [f64; 3],
    pub probe_footprint_bounds_city_enu_mm: CityBoundsMm,
    pub geometry_halo_bounds_city_enu_mm: CityBoundsMm,
    pub world: CityRouteWorldBinding,
    pub city_bake: CityRouteBakeBinding,
    pub echo_authority: Option<CityRouteEchoAuthorityBinding>,
    pub selection: CityRouteSelectionMetadata,
    pub prefetch: CityRoutePrefetchMetadata,
    pub installed_size: CityRouteInstalledSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CityRouteAdjacencyAxis {
    EastWest,
    NorthSouth,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteAdjacency {
    pub id: String,
    pub cells: [String; 2],
    pub axis: CityRouteAdjacencyAxis,
    pub overlap_bounds_city_enu_mm: CityBoundsMm,
    pub ownership_switch_coordinate_mm: i64,
    pub overlap_probe_count: u64,
    pub overlap_probes_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerHomeDesignation {
    pub cell_id: String,
    pub tier_id: String,
    pub probe_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FourCellFixturePlan {
    pub state: String,
    pub bakes_launched: bool,
    pub grid_min: CellGridIndex,
    pub grid_max: CellGridIndex,
    pub streamed_union_bounds_city_enu_mm: CityBoundsMm,
    pub monolithic_oracle_bounds_city_enu_mm: CityBoundsMm,
    pub monolithic_oracle_path_range_m: u32,
    pub streamed_cell_bakes_required: u32,
    pub monolithic_oracle_bakes_required: u32,
    pub seam_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteInstalledTotals {
    pub actual_installed_bytes: u64,
    pub projected_remaining_bake_bytes: u64,
    pub projected_complete_installed_bytes: u64,
    pub completed_cell_count: u32,
    pub planned_cell_count: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityRouteManifest {
    pub schema_version: String,
    pub assembler_revision: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub production_eligible: bool,
    pub city_id: String,
    pub route_id: String,
    pub route_order: String,
    pub grid_policy: CityRouteGridPolicy,
    pub cells: Vec<CityRouteCellRecord>,
    pub adjacencies: Vec<CityRouteAdjacency>,
    pub owner_home: Option<OwnerHomeDesignation>,
    pub four_cell_fixture: Option<FourCellFixturePlan>,
    pub installed_totals: CityRouteInstalledTotals,
}

impl CityRouteManifest {
    pub fn validate(&self) -> Result<()> {
        verify_constants()?;
        if self.schema_version != CITY_ROUTE_SCHEMA_ID
            || self.assembler_revision != CITY_ROUTE_ASSEMBLER_REVISION
            || self.route_order != "authored_sequence_at_485m_stride"
            || self.grid_policy != CityRouteGridPolicy::default()
        {
            return invalid("city-route manifest identity or grid policy is invalid");
        }
        validate_stable_id(&self.city_id, "city")?;
        validate_stable_id(&self.route_id, "route")?;
        if self.cells.is_empty() {
            return invalid("city-route manifest must contain at least one cell");
        }
        let mut ids = BTreeSet::new();
        let mut actual = 0_u64;
        let mut remaining = 0_u64;
        let mut completed = 0_u32;
        for (index, cell) in self.cells.iter().enumerate() {
            if cell.sequence as usize != index
                || cell.city_id != self.city_id
                || cell.cell_id != stable_cell_id(&self.city_id, cell.grid_index)
                || !ids.insert(cell.cell_id.as_str())
                || cell.selection.route_offset_mm != index as i64 * 485_000
            {
                return invalid("city-route cell order or identity is invalid");
            }
            validate_cell_record(cell)?;
            if let Some(completed_bake) = cell.city_bake.completed.as_ref() {
                let estimator_bound = completed_bake.probe_byte_estimate_v2_sha256.is_some()
                    && completed_bake.probe_byte_estimate_v2_size_bytes.is_some();
                if self.production_eligible != estimator_bound {
                    return invalid(
                        "completed route estimator identity must exactly match production eligibility",
                    );
                }
            }
            let expected_reverse = index
                .checked_sub(1)
                .map(|previous| self.cells[previous].cell_id.clone());
            let expected_forward = self.cells.get(index + 1).map(|next| next.cell_id.clone());
            if cell.prefetch.reverse_cell_id != expected_reverse
                || cell.prefetch.forward_cell_id != expected_forward
            {
                return invalid("city-route cell prefetch neighbors differ from route order");
            }
            actual = actual
                .checked_add(cell.installed_size.actual_installed_bytes)
                .ok_or_else(|| invalid_error("city-route actual installed total overflows"))?;
            remaining = remaining
                .checked_add(cell.installed_size.projected_remaining_bake_bytes)
                .ok_or_else(|| invalid_error("city-route projected remaining total overflows"))?;
            if cell.city_bake.state == CityRouteBakeState::BakedProbeBatch {
                completed = completed
                    .checked_add(1)
                    .ok_or_else(|| invalid_error("city-route completed cell count overflows"))?;
            }
        }
        for pair in self.cells.windows(2) {
            if grid_manhattan_distance(pair[0].grid_index, pair[1].grid_index) != 1 {
                return invalid("consecutive city-route cells must be grid-adjacent");
            }
            let resident_pair = pair
                .iter()
                .map(|cell| {
                    cell.echo_authority
                        .as_ref()
                        .map_or(0, |authority| authority.resident_size_bytes)
                })
                .try_fold(0_u64, |total, bytes| total.checked_add(bytes))
                .ok_or_else(|| invalid_error("echo authority resident pair overflows"))?;
            if resident_pair > MOBILE_ECHO_AUTHORITY_CAP_BYTES as u64 {
                return invalid(
                    "an active/prepared route-cell echo-authority pair exceeds the 64 MiB cap",
                );
            }
        }
        let expected_adjacencies = grid_adjacent_pairs(&self.cells);
        if self.adjacencies.len() != expected_adjacencies.len() {
            return invalid("city-route adjacency count differs from its cell grid");
        }
        for adjacency in &self.adjacencies {
            validate_adjacency_record(adjacency, &self.cells)?;
        }
        if let Some(owner_home) = &self.owner_home {
            let Some(cell) = self
                .cells
                .iter()
                .find(|cell| cell.cell_id == owner_home.cell_id)
            else {
                return invalid("owner-home cell is not present in the route");
            };
            if owner_home.tier_id != "owner-home"
                || owner_home.probe_count == 0
                || !cell
                    .city_bake
                    .tier_ids
                    .iter()
                    .any(|tier| tier == "owner-home")
            {
                return invalid("owner-home designation lacks an owner-home probe tier");
            }
        }
        if self.four_cell_fixture.is_some() != (self.cells.len() == 4) {
            return invalid("four-cell fixture must be present exactly for four-cell manifests");
        }
        if let Some(fixture) = &self.four_cell_fixture {
            validate_four_cell_fixture(fixture, self)?;
        }
        let projected = actual
            .checked_add(remaining)
            .ok_or_else(|| invalid_error("city-route projected complete total overflows"))?;
        if self.installed_totals
            != (CityRouteInstalledTotals {
                actual_installed_bytes: actual,
                projected_remaining_bake_bytes: remaining,
                projected_complete_installed_bytes: projected,
                completed_cell_count: completed,
                planned_cell_count: u32::try_from(self.cells.len())
                    .map_err(|_| invalid_error("city-route planned cell count exceeds u32"))?,
            })
        {
            return invalid("city-route installed totals differ from its cells");
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec_pretty(self)
            .map_err(|error| invalid_error(format!("serialize city-route manifest: {error}")))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| invalid_error(format!("city-route manifest JSON: {error}")))?;
        manifest.validate()?;
        Ok(manifest)
    }
}

pub fn assemble_city_route(request: CityRouteAssemblyRequest) -> Result<CityRouteManifest> {
    verify_constants()?;
    validate_stable_id(&request.route_id, "route")?;
    if request.cells.is_empty() || request.cells.len() > 4_096 {
        return invalid("city route must contain 1..=4096 cells");
    }
    let city_id = request.cells[0].city_id.clone();
    validate_stable_id(&city_id, "city")?;
    let placement_policy_sha256 = request.cells[0].probe_plan.placement_policy_sha256.clone();
    let mut ids = BTreeSet::new();
    for cell in &request.cells {
        validate_cell_input(cell, &city_id, &placement_policy_sha256)?;
        if !ids.insert(cell.cell_id.as_str()) {
            return invalid("city route contains a duplicate cell");
        }
    }
    for pair in request.cells.windows(2) {
        if grid_manhattan_distance(pair[0].grid_index, pair[1].grid_index) != 1 {
            return invalid("consecutive city-route inputs must be grid-adjacent");
        }
    }

    let adjacency_inputs = input_adjacent_pairs(&request.cells);
    let mut adjacencies = Vec::with_capacity(adjacency_inputs.len());
    for (left, right) in adjacency_inputs {
        adjacencies.push(build_adjacency(left, right)?);
    }
    adjacencies.sort_by(|left, right| left.id.cmp(&right.id));

    let mut cells = Vec::with_capacity(request.cells.len());
    for (index, input) in request.cells.iter().enumerate() {
        let reverse_cell_id = index
            .checked_sub(1)
            .map(|previous| request.cells[previous].cell_id.clone());
        let forward_cell_id = request
            .cells
            .get(index + 1)
            .map(|cell| cell.cell_id.clone());
        cells.push(build_cell_record(
            input,
            u32::try_from(index).map_err(|_| invalid_error("route sequence exceeds u32"))?,
            reverse_cell_id,
            forward_cell_id,
        )?);
    }

    let owner_home = request
        .owner_home_cell_id
        .as_deref()
        .map(|cell_id| build_owner_home(cell_id, &request.cells))
        .transpose()?;
    let four_cell_fixture = if request.include_four_cell_fixture {
        Some(build_four_cell_fixture(&cells, &adjacencies)?)
    } else {
        None
    };
    if request.cells.len() == 4 && four_cell_fixture.is_none() {
        return invalid("a four-cell route must explicitly include its seam/oracle fixture plan");
    }
    if request.cells.len() != 4 && four_cell_fixture.is_some() {
        return invalid("the seam/oracle fixture requires exactly four cells");
    }
    let installed_totals = installed_totals(&cells)?;
    let manifest = CityRouteManifest {
        schema_version: CITY_ROUTE_SCHEMA_ID.to_owned(),
        assembler_revision: CITY_ROUTE_ASSEMBLER_REVISION.to_owned(),
        production_eligible: true,
        city_id,
        route_id: request.route_id,
        route_order: "authored_sequence_at_485m_stride".to_owned(),
        grid_policy: CityRouteGridPolicy::default(),
        cells,
        adjacencies,
        owner_home,
        four_cell_fixture,
        installed_totals,
    };
    manifest.validate()?;
    Ok(manifest)
}

fn validate_cell_input(cell: &CityRouteCellInput, city_id: &str, policy_hash: &str) -> Result<()> {
    cell.probe_plan.validate()?;
    if cell.probe_plan.byte_estimate.path_horizon_m != 600
        || cell.probe_plan.byte_estimate.target_raw_probe_payload_bytes != 50_331_648
        || cell.probe_plan.byte_estimate.hard_raw_probe_payload_bytes != 67_108_864
        || !cell
            .probe_plan
            .byte_estimate
            .projected_high_within_hard_limit
        || cell.probe_plan.byte_estimate.projected_high_bytes > 67_108_864
    {
        return invalid(
            "city route cells must satisfy the canonical 600 m mobile probe-byte admission",
        );
    }
    if cell.city_id != city_id
        || cell.cell_id != stable_cell_id(city_id, cell.grid_index)
        || cell.grid_index != cell.probe_plan.cell.grid_index
        || cell.probe_plan.placement_policy_sha256 != policy_hash
    {
        return invalid("city route cell, plan, or placement-policy identity differs");
    }
    let expected_translation = cell
        .probe_plan
        .cell
        .local_to_city_enu_mm
        .map(|value| value as f64 / 1_000.0);
    if cell.local_to_city_enu_m != expected_translation {
        return invalid("city route world transform differs from its global probe slice");
    }
    for (hash, label) in [
        (&cell.world_manifest_sha256, "world manifest"),
        (&cell.mesh_sha256, "mesh"),
        (&cell.materials_sha256, "materials"),
        (&cell.probe_plan_sidecar_sha256, "probe plan sidecar"),
    ] {
        validate_sha256(hash, label)?;
    }
    if cell.installed_package_bytes == 0 {
        return invalid("city route package installed size must be positive");
    }
    if let Some(authority) = &cell.echo_authority {
        validate_echo_authority_input(authority)?;
    }
    match (&cell.completed_bake, &cell.completed_bake_sidecar_sha256) {
        (Some(bake), Some(sidecar_sha256)) => {
            bake.validate()?;
            validate_sha256(sidecar_sha256, "completed bake sidecar")?;
            let estimate_sha256 = cell.probe_byte_estimate_v2_sha256.as_ref().ok_or_else(|| {
                invalid_error(
                    "completed production route cell lacks probe-byte-estimate-v2 identity",
                )
            })?;
            validate_sha256(estimate_sha256, "probe-byte-estimate-v2")?;
            if cell.probe_byte_estimate_v2_size_bytes == Some(0)
                || cell.probe_byte_estimate_v2_size_bytes.is_none()
            {
                return invalid(
                    "completed production route cell lacks probe-byte-estimate-v2 size",
                );
            }
            if bake.city_id != cell.city_id
                || bake.cell_id != cell.cell_id
                || bake.cell_grid_index != cell.grid_index
                || bake.probe_plan_content_sha256 != cell.probe_plan_sidecar_sha256
                || bake.placement_policy_sha256 != cell.probe_plan.placement_policy_sha256
                || bake.probe_layout_sha256 != cell.probe_plan.probe_layout_sha256
                || bake.package_mesh_sha256 != cell.mesh_sha256
                || bake.package_materials_sha256 != cell.materials_sha256
                || cell.installed_bake_bytes == 0
            {
                return invalid("completed city bake does not bind to its route cell plan/package");
            }
        }
        (None, None)
            if cell.installed_bake_bytes == 0
                && cell.probe_byte_estimate_v2_sha256.is_none()
                && cell.probe_byte_estimate_v2_size_bytes.is_none() => {}
        _ => return invalid("city route completed bake fields must be present or absent together"),
    }
    Ok(())
}

fn build_cell_record(
    input: &CityRouteCellInput,
    sequence: u32,
    reverse_cell_id: Option<String>,
    forward_cell_id: Option<String>,
) -> Result<CityRouteCellRecord> {
    let completed = input
        .completed_bake
        .as_ref()
        .map(|bake| CityRouteCompletedBakeBinding {
            sidecar_sha256: input
                .completed_bake_sidecar_sha256
                .clone()
                .expect("validated completed sidecar hash"),
            probe_batch_sha256: bake.probe_batch.serialized_sha256.clone(),
            probe_batch_size_bytes: bake.probe_batch.serialized_size_bytes,
            path_data_size_bytes: bake.probe_batch.path_data_size_bytes,
            probe_byte_estimate_v2_sha256: input.probe_byte_estimate_v2_sha256.clone(),
            probe_byte_estimate_v2_size_bytes: input.probe_byte_estimate_v2_size_bytes,
        });
    let state = if completed.is_some() {
        CityRouteBakeState::BakedProbeBatch
    } else {
        CityRouteBakeState::ProbePlan
    };
    let tier_ids = input
        .probe_plan
        .tier_summaries
        .iter()
        .map(|tier| tier.id.clone())
        .collect::<Vec<_>>();
    let echo_authority =
        input
            .echo_authority
            .as_ref()
            .map(|authority| CityRouteEchoAuthorityBinding {
                capability: ECHO_AUTHORITY_CAPABILITY.to_owned(),
                package_sidecar_path: ECHO_AUTHORITY_SIDECAR_PATH.to_owned(),
                content_sha256: authority.content_sha256.clone(),
                serialized_size_bytes: authority.serialized_size_bytes,
                resident_size_bytes: authority.resident_size_bytes,
                anchor_set_sha256: authority.anchor_set_sha256.clone(),
                listener_layout_sha256: authority.listener_layout_sha256.clone(),
                coordinate_frame_key: authority.coordinate_frame_key.clone(),
                static_anchor_count: authority.static_anchor_count,
                residency_scope: "active_or_prepared_cell_only".to_owned(),
            });
    let projected_remaining_bake_bytes = if completed.is_some() {
        0
    } else {
        input.probe_plan.byte_estimate.projected_high_bytes
    };
    let actual_installed_bytes = input
        .installed_package_bytes
        .checked_add(input.installed_bake_bytes)
        .ok_or_else(|| invalid_error("city route cell installed size overflows"))?;
    let projected_complete_installed_bytes = actual_installed_bytes
        .checked_add(projected_remaining_bake_bytes)
        .ok_or_else(|| invalid_error("city route cell projected installed size overflows"))?;
    let raw_probe_bytes = completed.as_ref().map_or(
        input.probe_plan.byte_estimate.projected_high_bytes,
        |bake| bake.probe_batch_size_bytes,
    );
    let echo_resident_bytes = echo_authority
        .as_ref()
        .map_or(0, |authority| authority.resident_size_bytes);
    let echo_residency_expansion = echo_authority.as_ref().map_or(0, |authority| {
        authority
            .resident_size_bytes
            .saturating_sub(authority.serialized_size_bytes)
    });
    let prepared_resident_estimate_bytes = raw_probe_bytes
        .checked_mul(PROBE_RESIDENCY_MULTIPLIER_NUMERATOR)
        .and_then(|bytes| bytes.checked_add(PROBE_RESIDENCY_MULTIPLIER_DENOMINATOR - 1))
        .map(|bytes| bytes / PROBE_RESIDENCY_MULTIPLIER_DENOMINATOR)
        .and_then(|bytes| bytes.checked_add(input.installed_package_bytes))
        .and_then(|bytes| bytes.checked_add(echo_residency_expansion))
        .ok_or_else(|| invalid_error("city route prepared-resident estimate overflows"))?;
    Ok(CityRouteCellRecord {
        sequence,
        city_id: input.city_id.clone(),
        cell_id: input.cell_id.clone(),
        grid_index: input.grid_index,
        local_to_city_enu_m: input.local_to_city_enu_m,
        probe_footprint_bounds_city_enu_mm: input
            .probe_plan
            .cell
            .probe_footprint_bounds_city_enu_mm,
        geometry_halo_bounds_city_enu_mm: input
            .probe_plan
            .cell
            .geometry_material_bounds_city_enu_mm,
        world: CityRouteWorldBinding {
            manifest_sha256: input.world_manifest_sha256.clone(),
            mesh_sha256: input.mesh_sha256.clone(),
            materials_sha256: input.materials_sha256.clone(),
        },
        city_bake: CityRouteBakeBinding {
            state,
            probe_plan_sidecar_sha256: input.probe_plan_sidecar_sha256.clone(),
            placement_policy_sha256: input.probe_plan.placement_policy_sha256.clone(),
            probe_layout_sha256: input.probe_plan.probe_layout_sha256.clone(),
            probe_count: input.probe_plan.byte_estimate.probe_count,
            tier_ids,
            maximum_layer_m: input.probe_plan.policy.sky_pathing_policy.maximum_layer_m,
            above_maximum_layer: input.probe_plan.policy.sky_pathing_policy.above,
            calibrated_model: input
                .completed_bake
                .as_ref()
                .and_then(|bake| bake.calibrated_model.clone()),
            completed,
        },
        echo_authority,
        selection: CityRouteSelectionMetadata {
            route_offset_mm: i64::from(sequence) * 485_000,
            ownership_bounds_city_enu_mm: input.probe_plan.cell.ownership_bounds_city_enu_mm,
            guard_bounds_city_enu_mm: input.probe_plan.cell.probe_footprint_bounds_city_enu_mm,
        },
        prefetch: CityRoutePrefetchMetadata {
            reverse_cell_id,
            forward_cell_id,
            raw_cell_bytes: raw_probe_bytes,
            prepared_resident_estimate_bytes,
            preparation_scratch_bytes: raw_probe_bytes,
            echo_authority_resident_bytes: echo_resident_bytes,
            echo_authority_load_policy: if echo_resident_bytes == 0 {
                "not_installed"
            } else {
                "active_or_prepared_cell_only"
            }
            .to_owned(),
            estimate_revision: "probe-bytes-x3.29-plus-package-plus-cell-echo-v2".to_owned(),
        },
        installed_size: CityRouteInstalledSize {
            package_bytes: input.installed_package_bytes,
            baked_artifact_bytes: input.installed_bake_bytes,
            actual_installed_bytes,
            projected_remaining_bake_bytes,
            projected_complete_installed_bytes,
        },
    })
}

fn validate_cell_record(cell: &CityRouteCellRecord) -> Result<()> {
    let expected_slice = CellProbeSlice::for_grid_index(cell.grid_index)?;
    for (hash, label) in [
        (&cell.world.manifest_sha256, "world manifest"),
        (&cell.world.mesh_sha256, "mesh"),
        (&cell.world.materials_sha256, "materials"),
        (
            &cell.city_bake.probe_plan_sidecar_sha256,
            "probe plan sidecar",
        ),
        (&cell.city_bake.placement_policy_sha256, "placement policy"),
        (&cell.city_bake.probe_layout_sha256, "probe layout"),
    ] {
        validate_sha256(hash, label)?;
    }
    let expected_translation = [
        f64::from(cell.grid_index.east) * CELL_STRIDE_M,
        f64::from(cell.grid_index.north) * CELL_STRIDE_M,
        0.0,
    ];
    if cell.local_to_city_enu_m != expected_translation
        || cell.probe_footprint_bounds_city_enu_mm
            != expected_slice.probe_footprint_bounds_city_enu_mm
        || cell.selection.ownership_bounds_city_enu_mm
            != expected_slice.ownership_bounds_city_enu_mm
        || cell.selection.guard_bounds_city_enu_mm
            != expected_slice.probe_footprint_bounds_city_enu_mm
        || cell.geometry_halo_bounds_city_enu_mm
            != expected_slice.geometry_material_bounds_city_enu_mm
        || cell.city_bake.probe_count == 0
        || cell.city_bake.maximum_layer_m != 63
        || cell.city_bake.above_maximum_layer != AboveMaximumLayerPolicy::DirectReflectionsOnly
        || cell.prefetch.raw_cell_bytes == 0
        || cell.prefetch.prepared_resident_estimate_bytes == 0
        || cell.prefetch.preparation_scratch_bytes == 0
        || cell.prefetch.estimate_revision != "probe-bytes-x3.29-plus-package-plus-cell-echo-v2"
    {
        return invalid("city-route cell payload or stream metadata is invalid");
    }
    match &cell.echo_authority {
        Some(authority) => {
            validate_echo_authority_binding(authority)?;
            if cell.prefetch.echo_authority_resident_bytes != authority.resident_size_bytes
                || cell.prefetch.echo_authority_load_policy != "active_or_prepared_cell_only"
            {
                return invalid("city-route echo authority prefetch metadata differs from binding");
            }
        }
        None => {
            if cell.prefetch.echo_authority_resident_bytes != 0
                || cell.prefetch.echo_authority_load_policy != "not_installed"
            {
                return invalid("echo-free route cell carries authority residency metadata");
            }
        }
    }
    if cell.installed_size.actual_installed_bytes
        != cell
            .installed_size
            .package_bytes
            .checked_add(cell.installed_size.baked_artifact_bytes)
            .ok_or_else(|| invalid_error("city-route cell installed size overflows"))?
        || cell.installed_size.projected_complete_installed_bytes
            != cell
                .installed_size
                .actual_installed_bytes
                .checked_add(cell.installed_size.projected_remaining_bake_bytes)
                .ok_or_else(|| invalid_error("city-route cell projected size overflows"))?
    {
        return invalid("city-route cell installed size fields disagree");
    }
    match (&cell.city_bake.calibrated_model, &cell.city_bake.completed) {
        (Some(model), Some(_completed)) => {
            model.validate()?;
            if model.mesh_observable.mesh_sha256.as_deref() != Some(cell.world.mesh_sha256.as_str())
            {
                return invalid(
                    "city-route calibrated mesh observable is not bound to the world mesh",
                );
            }
            let tier_total = model
                .owner_home_probe_count
                .checked_add(model.route_core_probe_count)
                .and_then(|value| value.checked_add(model.transition_probe_count))
                .and_then(|value| value.checked_add(model.residual_probe_count))
                .ok_or_else(|| invalid_error("city-route calibrated tier count overflows"))?;
            if tier_total != cell.city_bake.probe_count {
                return invalid(
                    "city-route calibrated tier counts differ from the planned probe count",
                );
            }
        }
        (None, Some(_completed)) => {}
        (Some(_), None) => return invalid("city-route calibrated model requires a completed bake"),
        (None, None) => {}
    }
    match (cell.city_bake.state, &cell.city_bake.completed) {
        (CityRouteBakeState::ProbePlan, None)
            if cell.installed_size.baked_artifact_bytes == 0
                && cell.installed_size.projected_remaining_bake_bytes > 0 => {}
        (CityRouteBakeState::BakedProbeBatch, Some(completed))
            if cell.installed_size.baked_artifact_bytes > 0
                && cell.installed_size.projected_remaining_bake_bytes == 0 =>
        {
            validate_sha256(&completed.sidecar_sha256, "completed bake sidecar")?;
            validate_sha256(&completed.probe_batch_sha256, "probe batch")?;
            if completed.probe_batch_size_bytes == 0 || completed.path_data_size_bytes == 0 {
                return invalid("completed city-route bake sizes must be positive");
            }
        }
        _ => return invalid("city-route bake state and completed binding disagree"),
    }
    Ok(())
}

fn validate_echo_authority_input(authority: &CityRouteEchoAuthorityInput) -> Result<()> {
    validate_echo_authority_fields(
        &authority.content_sha256,
        authority.serialized_size_bytes,
        authority.resident_size_bytes,
        &authority.anchor_set_sha256,
        &authority.listener_layout_sha256,
        &authority.coordinate_frame_key,
        authority.static_anchor_count,
    )
}

fn validate_echo_authority_binding(authority: &CityRouteEchoAuthorityBinding) -> Result<()> {
    if authority.capability != ECHO_AUTHORITY_CAPABILITY
        || authority.package_sidecar_path != ECHO_AUTHORITY_SIDECAR_PATH
        || authority.residency_scope != "active_or_prepared_cell_only"
    {
        return invalid("city-route echo authority capability or residency policy is invalid");
    }
    validate_echo_authority_fields(
        &authority.content_sha256,
        authority.serialized_size_bytes,
        authority.resident_size_bytes,
        &authority.anchor_set_sha256,
        &authority.listener_layout_sha256,
        &authority.coordinate_frame_key,
        authority.static_anchor_count,
    )
}

fn validate_echo_authority_fields(
    content_sha256: &str,
    serialized_size_bytes: u64,
    resident_size_bytes: u64,
    anchor_set_sha256: &str,
    listener_layout_sha256: &str,
    coordinate_frame_key: &str,
    static_anchor_count: u32,
) -> Result<()> {
    for (hash, label) in [
        (content_sha256, "echo authority content"),
        (anchor_set_sha256, "echo authority anchor set"),
        (listener_layout_sha256, "echo authority listener layout"),
    ] {
        validate_sha256(hash, label)?;
    }
    if coordinate_frame_key.len() != 32
        || !coordinate_frame_key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || serialized_size_bytes == 0
        || resident_size_bytes == 0
        || serialized_size_bytes > MOBILE_ECHO_AUTHORITY_CAP_BYTES as u64
        || resident_size_bytes > MOBILE_ECHO_AUTHORITY_CAP_BYTES as u64
        || static_anchor_count == 0
    {
        return invalid("city-route echo authority identity, size, or anchor count is invalid");
    }
    Ok(())
}

fn build_adjacency(
    left: &CityRouteCellInput,
    right: &CityRouteCellInput,
) -> Result<CityRouteAdjacency> {
    let (first, second) = if left.cell_id < right.cell_id {
        (left, right)
    } else {
        (right, left)
    };
    let east_delta = (i64::from(first.grid_index.east) - i64::from(second.grid_index.east)).abs();
    let north_delta =
        (i64::from(first.grid_index.north) - i64::from(second.grid_index.north)).abs();
    if east_delta + north_delta != 1 {
        return invalid("city-route adjacency inputs are not grid-adjacent");
    }
    let axis = if east_delta == 1 {
        CityRouteAdjacencyAxis::EastWest
    } else {
        CityRouteAdjacencyAxis::NorthSouth
    };
    let overlap = first
        .probe_plan
        .cell
        .probe_footprint_bounds_city_enu_mm
        .intersection(second.probe_plan.cell.probe_footprint_bounds_city_enu_mm)
        .ok_or_else(|| invalid_error("adjacent city cells have no probe overlap"))?;
    let overlap_extent = [
        overlap.max[0] - overlap.min[0],
        overlap.max[1] - overlap.min[1],
    ];
    let expected = match axis {
        CityRouteAdjacencyAxis::EastWest => [100_000, 585_000],
        CityRouteAdjacencyAxis::NorthSouth => [585_000, 100_000],
    };
    if overlap_extent != expected {
        return invalid("adjacent city-cell overlap does not match the 100 m contract");
    }
    let first_probes = probes_in_bounds(&first.probe_plan.probes, overlap);
    let second_probes = probes_in_bounds(&second.probe_plan.probes, overlap);
    if first_probes.is_empty() || first_probes != second_probes {
        return invalid("adjacent city cells do not contain identical overlap probe records");
    }
    let overlap_probe_bytes = serde_json::to_vec(&first_probes)
        .map_err(|error| invalid_error(format!("serialize overlap probes: {error}")))?;
    let switch = match axis {
        CityRouteAdjacencyAxis::EastWest => {
            let west = if first.grid_index.east < second.grid_index.east {
                first
            } else {
                second
            };
            let east = if west.grid_index == first.grid_index {
                second
            } else {
                first
            };
            let coordinate = west.probe_plan.cell.ownership_bounds_city_enu_mm.max[0];
            if coordinate != east.probe_plan.cell.ownership_bounds_city_enu_mm.min[0] {
                return invalid("east-west city-cell ownership bounds are discontinuous");
            }
            coordinate
        }
        CityRouteAdjacencyAxis::NorthSouth => {
            let south = if first.grid_index.north < second.grid_index.north {
                first
            } else {
                second
            };
            let north = if south.grid_index == first.grid_index {
                second
            } else {
                first
            };
            let coordinate = south.probe_plan.cell.ownership_bounds_city_enu_mm.max[1];
            if coordinate != north.probe_plan.cell.ownership_bounds_city_enu_mm.min[1] {
                return invalid("north-south city-cell ownership bounds are discontinuous");
            }
            coordinate
        }
    };
    Ok(CityRouteAdjacency {
        id: format!("{}--{}", first.cell_id, second.cell_id),
        cells: [first.cell_id.clone(), second.cell_id.clone()],
        axis,
        overlap_bounds_city_enu_mm: overlap,
        ownership_switch_coordinate_mm: switch,
        overlap_probe_count: u64::try_from(first_probes.len())
            .map_err(|_| invalid_error("overlap probe count exceeds u64"))?,
        overlap_probes_sha256: sha256_hex(&overlap_probe_bytes),
    })
}

fn validate_adjacency_record(
    adjacency: &CityRouteAdjacency,
    cells: &[CityRouteCellRecord],
) -> Result<()> {
    validate_sha256(&adjacency.overlap_probes_sha256, "overlap probes")?;
    if adjacency.overlap_probe_count == 0
        || adjacency.id != format!("{}--{}", adjacency.cells[0], adjacency.cells[1])
        || adjacency.cells[0] >= adjacency.cells[1]
    {
        return invalid("city-route adjacency identity or probe count is invalid");
    }
    let by_id = cells
        .iter()
        .map(|cell| (cell.cell_id.as_str(), cell))
        .collect::<BTreeMap<_, _>>();
    let left = by_id
        .get(adjacency.cells[0].as_str())
        .ok_or_else(|| invalid_error("city-route adjacency references a missing cell"))?;
    let right = by_id
        .get(adjacency.cells[1].as_str())
        .ok_or_else(|| invalid_error("city-route adjacency references a missing cell"))?;
    if grid_manhattan_distance(left.grid_index, right.grid_index) != 1 {
        return invalid("city-route adjacency references non-adjacent cells");
    }
    let expected_axis = if left.grid_index.east != right.grid_index.east {
        CityRouteAdjacencyAxis::EastWest
    } else {
        CityRouteAdjacencyAxis::NorthSouth
    };
    let overlap = left
        .probe_footprint_bounds_city_enu_mm
        .intersection(right.probe_footprint_bounds_city_enu_mm)
        .ok_or_else(|| invalid_error("city-route adjacency has no overlap"))?;
    let expected_switch = match expected_axis {
        CityRouteAdjacencyAxis::EastWest => {
            let west = if left.grid_index.east < right.grid_index.east {
                left
            } else {
                right
            };
            west.selection.ownership_bounds_city_enu_mm.max[0]
        }
        CityRouteAdjacencyAxis::NorthSouth => {
            let south = if left.grid_index.north < right.grid_index.north {
                left
            } else {
                right
            };
            south.selection.ownership_bounds_city_enu_mm.max[1]
        }
    };
    if adjacency.axis != expected_axis
        || adjacency.overlap_bounds_city_enu_mm != overlap
        || adjacency.ownership_switch_coordinate_mm != expected_switch
    {
        return invalid("city-route adjacency geometry differs from its cells");
    }
    Ok(())
}

fn build_owner_home(cell_id: &str, cells: &[CityRouteCellInput]) -> Result<OwnerHomeDesignation> {
    let cell = cells
        .iter()
        .find(|cell| cell.cell_id == cell_id)
        .ok_or_else(|| invalid_error("owner-home cell is not present in the route"))?;
    let tier = cell
        .probe_plan
        .tier_summaries
        .iter()
        .find(|tier| tier.id == "owner-home")
        .ok_or_else(|| invalid_error("owner-home cell has no owner-home probe tier"))?;
    if tier.probe_count == 0 {
        return invalid("owner-home probe tier is empty");
    }
    Ok(OwnerHomeDesignation {
        cell_id: cell.cell_id.clone(),
        tier_id: tier.id.clone(),
        probe_count: tier.probe_count,
    })
}

fn build_four_cell_fixture(
    cells: &[CityRouteCellRecord],
    adjacencies: &[CityRouteAdjacency],
) -> Result<FourCellFixturePlan> {
    if cells.len() != 4 {
        return invalid("four-cell fixture requires exactly four cells");
    }
    let min_east = cells.iter().map(|cell| cell.grid_index.east).min().unwrap();
    let max_east = cells.iter().map(|cell| cell.grid_index.east).max().unwrap();
    let min_north = cells
        .iter()
        .map(|cell| cell.grid_index.north)
        .min()
        .unwrap();
    let max_north = cells
        .iter()
        .map(|cell| cell.grid_index.north)
        .max()
        .unwrap();
    if max_east - min_east != 1 || max_north - min_north != 1 || adjacencies.len() != 4 {
        return invalid("four-cell fixture must form one contiguous 2x2 grid with four seams");
    }
    let expected = [
        CellGridIndex {
            east: min_east,
            north: min_north,
        },
        CellGridIndex {
            east: max_east,
            north: min_north,
        },
        CellGridIndex {
            east: min_east,
            north: max_north,
        },
        CellGridIndex {
            east: max_east,
            north: max_north,
        },
    ];
    if !expected
        .iter()
        .all(|index| cells.iter().any(|cell| cell.grid_index == *index))
    {
        return invalid("four-cell fixture grid has a missing corner");
    }
    let streamed_union = union_bounds(
        cells
            .iter()
            .map(|cell| cell.probe_footprint_bounds_city_enu_mm),
    )?;
    let oracle = CityBoundsMm::new(
        [
            streamed_union.min[0] - FOUR_CELL_ORACLE_MARGIN_MM,
            streamed_union.min[1] - FOUR_CELL_ORACLE_MARGIN_MM,
        ],
        [
            streamed_union.max[0] + FOUR_CELL_ORACLE_MARGIN_MM,
            streamed_union.max[1] + FOUR_CELL_ORACLE_MARGIN_MM,
        ],
    );
    if oracle.max[0] - oracle.min[0] != 1_170_000 || oracle.max[1] - oracle.min[1] != 1_170_000 {
        return invalid("four-cell monolithic oracle footprint must be 1.17 km square");
    }
    let completed_streamed_bakes = cells
        .iter()
        .filter(|cell| cell.city_bake.state == CityRouteBakeState::BakedProbeBatch)
        .count();
    let state = match completed_streamed_bakes {
        0 => "plan_only",
        4 => "streamed_cell_bakes_complete_oracle_pending",
        _ => "streamed_cell_bakes_incomplete",
    };
    Ok(FourCellFixturePlan {
        state: state.to_owned(),
        bakes_launched: completed_streamed_bakes != 0,
        grid_min: CellGridIndex {
            east: min_east,
            north: min_north,
        },
        grid_max: CellGridIndex {
            east: max_east,
            north: max_north,
        },
        streamed_union_bounds_city_enu_mm: streamed_union,
        monolithic_oracle_bounds_city_enu_mm: oracle,
        monolithic_oracle_path_range_m: FOUR_CELL_ORACLE_PATH_RANGE_M,
        streamed_cell_bakes_required: 4,
        monolithic_oracle_bakes_required: 1,
        seam_ids: adjacencies
            .iter()
            .map(|adjacency| adjacency.id.clone())
            .collect(),
    })
}

fn validate_four_cell_fixture(
    fixture: &FourCellFixturePlan,
    manifest: &CityRouteManifest,
) -> Result<()> {
    let expected = build_four_cell_fixture(&manifest.cells, &manifest.adjacencies)?;
    if fixture.monolithic_oracle_path_range_m != FOUR_CELL_ORACLE_PATH_RANGE_M
        || fixture.streamed_cell_bakes_required != 4
        || fixture.monolithic_oracle_bakes_required != 1
        || fixture.seam_ids
            != manifest
                .adjacencies
                .iter()
                .map(|adjacency| adjacency.id.clone())
                .collect::<Vec<_>>()
        || fixture != &expected
    {
        return invalid("four-cell seam/oracle fixture metadata is invalid");
    }
    Ok(())
}

fn installed_totals(cells: &[CityRouteCellRecord]) -> Result<CityRouteInstalledTotals> {
    let actual = cells
        .iter()
        .try_fold(0_u64, |total, cell| {
            total.checked_add(cell.installed_size.actual_installed_bytes)
        })
        .ok_or_else(|| invalid_error("city-route installed total overflows"))?;
    let remaining = cells
        .iter()
        .try_fold(0_u64, |total, cell| {
            total.checked_add(cell.installed_size.projected_remaining_bake_bytes)
        })
        .ok_or_else(|| invalid_error("city-route projected remaining total overflows"))?;
    Ok(CityRouteInstalledTotals {
        actual_installed_bytes: actual,
        projected_remaining_bake_bytes: remaining,
        projected_complete_installed_bytes: actual
            .checked_add(remaining)
            .ok_or_else(|| invalid_error("city-route projected complete total overflows"))?,
        completed_cell_count: u32::try_from(
            cells
                .iter()
                .filter(|cell| cell.city_bake.state == CityRouteBakeState::BakedProbeBatch)
                .count(),
        )
        .map_err(|_| invalid_error("city-route completed count exceeds u32"))?,
        planned_cell_count: u32::try_from(cells.len())
            .map_err(|_| invalid_error("city-route planned count exceeds u32"))?,
    })
}

fn input_adjacent_pairs(
    cells: &[CityRouteCellInput],
) -> Vec<(&CityRouteCellInput, &CityRouteCellInput)> {
    let mut pairs = Vec::new();
    for (index, left) in cells.iter().enumerate() {
        for right in &cells[index + 1..] {
            if grid_manhattan_distance(left.grid_index, right.grid_index) == 1 {
                pairs.push((left, right));
            }
        }
    }
    pairs
}

fn grid_adjacent_pairs(
    cells: &[CityRouteCellRecord],
) -> Vec<(&CityRouteCellRecord, &CityRouteCellRecord)> {
    let mut pairs = Vec::new();
    for (index, left) in cells.iter().enumerate() {
        for right in &cells[index + 1..] {
            if grid_manhattan_distance(left.grid_index, right.grid_index) == 1 {
                pairs.push((left, right));
            }
        }
    }
    pairs
}

fn grid_manhattan_distance(left: CellGridIndex, right: CellGridIndex) -> i64 {
    (i64::from(left.east) - i64::from(right.east)).abs()
        + (i64::from(left.north) - i64::from(right.north)).abs()
}

fn probes_in_bounds(probes: &[ProbeSite], bounds: CityBoundsMm) -> Vec<ProbeSite> {
    probes
        .iter()
        .filter(|probe| {
            bounds.contains_closed([probe.center_city_enu_mm[0], probe.center_city_enu_mm[1]])
        })
        .cloned()
        .collect()
}

fn union_bounds(bounds: impl Iterator<Item = CityBoundsMm>) -> Result<CityBoundsMm> {
    let mut bounds = bounds;
    let first = bounds
        .next()
        .ok_or_else(|| invalid_error("cannot union an empty city bounds set"))?;
    Ok(bounds.fold(first, |union, next| {
        CityBoundsMm::new(
            [union.min[0].min(next.min[0]), union.min[1].min(next.min[1])],
            [union.max[0].max(next.max[0]), union.max[1].max(next.max[1])],
        )
    }))
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid(format!("{label} SHA-256 must be lowercase hexadecimal"));
    }
    Ok(())
}

fn validate_stable_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b'/')
        })
    {
        return invalid(format!("{label} ID is not a lowercase stable identifier"));
    }
    Ok(())
}

fn verify_constants() -> Result<()> {
    if CELL_PROBE_FOOTPRINT_M.to_bits() != 585.0_f64.to_bits()
        || CELL_STRIDE_M.to_bits() != 485.0_f64.to_bits()
        || CELL_PAIRWISE_OVERLAP_M.to_bits() != 100.0_f64.to_bits()
        || CELL_OWNERSHIP_GUARD_M.to_bits() != 50.0_f64.to_bits()
        || CELL_GEOMETRY_HALO_M.to_bits() != 600.0_f64.to_bits()
        || MILLIMETRES_PER_METRE != 1_000
    {
        return invalid("city-route constants differ from world-package-v2");
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(invalid_error(message))
}

fn invalid_error(message: impl Into<String>) -> WorldError {
    WorldError::InvalidPackage(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ElevatedProbeLayerPolicy, GradedProbePolicy, ProbeTierPolicy};

    fn region(
        min_east_m: i64,
        min_north_m: i64,
        max_east_m: i64,
        max_north_m: i64,
    ) -> CityBoundsMm {
        CityBoundsMm::new(
            [min_east_m * 1_000, min_north_m * 1_000],
            [max_east_m * 1_000, max_north_m * 1_000],
        )
    }

    fn elevated(id: &str, up_m: i64, spacing_m: u32) -> ElevatedProbeLayerPolicy {
        ElevatedProbeLayerPolicy {
            id: id.to_owned(),
            up_mm: up_m * 1_000,
            spacing_m,
        }
    }

    fn fixture_policy() -> GradedProbePolicy {
        GradedProbePolicy::new(
            [7_500, 7_500],
            vec![
                ProbeTierPolicy {
                    id: "residual".to_owned(),
                    ground_up_mm: 1_500,
                    ground_spacing_m: 32,
                    analysis_spacing_m: 32,
                    regions_city_enu_mm: vec![region(-2_000, -2_000, 2_000, 2_000)],
                    elevated_layers: vec![],
                },
                ProbeTierPolicy {
                    id: "route-core".to_owned(),
                    ground_up_mm: 1_500,
                    ground_spacing_m: 8,
                    analysis_spacing_m: 8,
                    regions_city_enu_mm: vec![
                        region(-2_000, -24, 2_000, 24),
                        region(-58, -58, 58, 58),
                    ],
                    elevated_layers: vec![
                        elevated("route-63m", 63, 16),
                        elevated("route-30m", 30, 16),
                    ],
                },
                ProbeTierPolicy {
                    id: "owner-home".to_owned(),
                    ground_up_mm: 1_500,
                    ground_spacing_m: 4,
                    analysis_spacing_m: 4,
                    regions_city_enu_mm: vec![region(-50, -50, 50, 50)],
                    elevated_layers: vec![],
                },
                ProbeTierPolicy {
                    id: "transition".to_owned(),
                    ground_up_mm: 1_500,
                    ground_spacing_m: 16,
                    analysis_spacing_m: 16,
                    regions_city_enu_mm: vec![
                        region(-2_000, -64, 2_000, 64),
                        region(-74, -74, 74, 74),
                    ],
                    elevated_layers: vec![
                        elevated("transition-30m", 30, 32),
                        elevated("transition-63m", 63, 32),
                    ],
                },
            ],
        )
    }

    fn fixture_cell(policy: &GradedProbePolicy, grid_index: CellGridIndex) -> CityRouteCellInput {
        let plan = policy.plan_cell(grid_index).unwrap();
        let sidecar = plan.to_sidecar_bytes().unwrap();
        let cell_id = stable_cell_id("fixture-city", grid_index);
        CityRouteCellInput {
            city_id: "fixture-city".to_owned(),
            cell_id,
            grid_index,
            local_to_city_enu_m: plan
                .cell
                .local_to_city_enu_mm
                .map(|millimetres| millimetres as f64 / 1_000.0),
            world_manifest_sha256: "1".repeat(64),
            mesh_sha256: "2".repeat(64),
            materials_sha256: "3".repeat(64),
            probe_plan_sidecar_sha256: sha256_hex(&sidecar),
            probe_plan: plan,
            completed_bake_sidecar_sha256: None,
            completed_bake: None,
            probe_byte_estimate_v2_sha256: None,
            probe_byte_estimate_v2_size_bytes: None,
            echo_authority: None,
            installed_package_bytes: 2 * 1_024 * 1_024,
            installed_bake_bytes: 0,
        }
    }

    #[test]
    fn four_cell_route_codec_preserves_overlap_and_prefetch_contracts() {
        let policy = fixture_policy();
        let cells = [
            CellGridIndex { east: 0, north: 0 },
            CellGridIndex { east: 1, north: 0 },
            CellGridIndex { east: 1, north: 1 },
            CellGridIndex { east: 0, north: 1 },
        ]
        .into_iter()
        .map(|index| {
            let mut cell = fixture_cell(&policy, index);
            cell.echo_authority = Some(CityRouteEchoAuthorityInput {
                content_sha256: "4".repeat(64),
                serialized_size_bytes: 1 * 1_024 * 1_024,
                resident_size_bytes: 8 * 1_024 * 1_024,
                anchor_set_sha256: "5".repeat(64),
                listener_layout_sha256: "6".repeat(64),
                coordinate_frame_key: "7".repeat(32),
                static_anchor_count: 1,
            });
            cell
        })
        .collect::<Vec<_>>();
        let owner_home_cell_id = cells[0].cell_id.clone();
        let manifest = assemble_city_route(CityRouteAssemblyRequest {
            route_id: "gamma-four-cell".to_owned(),
            cells,
            owner_home_cell_id: Some(owner_home_cell_id.clone()),
            include_four_cell_fixture: true,
        })
        .unwrap();

        assert_eq!(manifest.cells.len(), 4);
        assert_eq!(manifest.adjacencies.len(), 4);
        assert!(
            manifest
                .adjacencies
                .iter()
                .all(|adjacency| adjacency.overlap_probe_count > 0)
        );
        assert_eq!(manifest.cells[0].prefetch.reverse_cell_id, None);
        let echo = manifest.cells[0].echo_authority.as_ref().unwrap();
        assert_eq!(echo.capability, ECHO_AUTHORITY_CAPABILITY);
        assert_eq!(echo.package_sidecar_path, ECHO_AUTHORITY_SIDECAR_PATH);
        assert_eq!(echo.residency_scope, "active_or_prepared_cell_only");
        assert_eq!(
            manifest.cells[0].prefetch.echo_authority_resident_bytes,
            8 * 1_024 * 1_024
        );
        assert_eq!(
            manifest.cells[0].prefetch.forward_cell_id.as_deref(),
            Some(manifest.cells[1].cell_id.as_str())
        );
        assert_eq!(
            manifest.owner_home.as_ref().unwrap().cell_id,
            owner_home_cell_id
        );
        let fixture = manifest.four_cell_fixture.as_ref().unwrap();
        assert!(!fixture.bakes_launched);
        assert_eq!(
            fixture.streamed_union_bounds_city_enu_mm.max[0]
                - fixture.streamed_union_bounds_city_enu_mm.min[0],
            1_070_000
        );
        assert_eq!(
            fixture.monolithic_oracle_bounds_city_enu_mm.max[0]
                - fixture.monolithic_oracle_bounds_city_enu_mm.min[0],
            1_170_000
        );

        let bytes = manifest.to_bytes().unwrap();
        assert_eq!(CityRouteManifest::from_bytes(&bytes).unwrap(), manifest);
    }
    #[test]
    fn route_assembly_rejects_a_valid_over_hard_mobile_plan() {
        let policy = GradedProbePolicy::new(
            [0, 0],
            vec![
                ProbeTierPolicy {
                    id: "calibration-grid".to_owned(),
                    ground_up_mm: 1_500,
                    ground_spacing_m: 4,
                    analysis_spacing_m: 4,
                    regions_city_enu_mm: vec![CityBoundsMm::new(
                        [-96_000, -96_000],
                        [92_000, 92_000],
                    )],
                    elevated_layers: vec![],
                },
                ProbeTierPolicy {
                    id: "calibration-maximum-layer".to_owned(),
                    ground_up_mm: 1_500,
                    ground_spacing_m: 4,
                    analysis_spacing_m: 4,
                    regions_city_enu_mm: vec![CityBoundsMm::new([-1_000, -1_000], [1_000, 1_000])],
                    elevated_layers: vec![elevated("calibration-63m", 63, 4)],
                },
            ],
        );
        let cell = fixture_cell(&policy, CellGridIndex { east: 0, north: 0 });
        assert!(
            !cell
                .probe_plan
                .byte_estimate
                .projected_high_within_hard_limit
        );
        let error = assemble_city_route(CityRouteAssemblyRequest {
            route_id: "over-hard".to_owned(),
            cells: vec![cell],
            owner_home_cell_id: None,
            include_four_cell_fixture: false,
        })
        .expect_err("over-hard plan must not enter route authority");
        assert!(error.to_string().contains("mobile probe-byte admission"));
    }
}
