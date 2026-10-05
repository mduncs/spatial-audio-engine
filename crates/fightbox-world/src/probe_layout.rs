use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

use fightbox_api::EnuVector3;

use crate::{
    AcousticMesh, CapabilityExtension, CellGridIndex, ExtensionRequirement, Result, WorldError,
    package_v2::{
        CELL_GEOMETRY_HALO_M, CELL_OWNERSHIP_GUARD_M, CELL_PAIRWISE_OVERLAP_M,
        CELL_PROBE_FOOTPRINT_M, CELL_STRIDE_M, MOBILE_BAKED_PATH_HORIZON_M,
        MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES, MOBILE_TARGET_RAW_PROBE_PAYLOAD_BYTES,
    },
    sha256::sha256_hex,
};

pub const CITY_BAKE_V2_CAPABILITY: &str = "fightbox.city-bake.v2";
pub const CITY_BAKE_V2_SIDECAR_PATH: &str = "capabilities/city-bake-v2.json";
pub const CITY_BAKE_V2_SCHEMA_ID: &str = "fightbox.city-bake.v2";
pub const PROBE_LAYOUT_ESTIMATOR_REVISION: &str = "reachable-ordered-pairs-p256-q10-fixed64k-v1";
/// Additive production estimator revision.  The legacy revision above remains
/// the planner default and is intentionally byte-compatible with existing
/// plan and bake sidecars.
pub const FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION: &str =
    "wave17-fixed-tier-mesh-open-pairs-v2";
pub const FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM: &str =
    "canonical-package-mesh-owner-ground-open-pairs-v2";
pub const FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING: &str = "signed_integer_millimetres";
pub const CALIBRATED_MOBILE_PLACEMENT_POLICY_SHA256: &str =
    "b798fc0eb39a9f8da5c14595b0452dc38e798bc35c5da7fc95035da09f2fca0a";
pub const MAXIMUM_PROBE_LAYER_M: u32 = 63;

const MILLIMETRES_PER_METRE: i64 = 1_000;
const CELL_PROBE_FOOTPRINT_MM: i64 = 585_000;
const CELL_STRIDE_MM: i64 = 485_000;
const CELL_PAIRWISE_OVERLAP_MM: i64 = 100_000;
const CELL_OWNERSHIP_GUARD_MM: i64 = 50_000;
const CELL_GEOMETRY_HALO_MM: i64 = 600_000;
const SERIALIZATION_FIXED_BYTES: u64 = 64 * 1_024;
const PROBE_BYTES: u64 = 256;
const REACHABLE_ORDERED_PAIR_BYTES: u64 = 10;

const COORDINATE_FRAME: &str = "city_enu_m";
const COORDINATE_ENCODING: &str = "signed_millimetres";
const CELL_BOUNDS_RULE: &str = "closed_probe_footprint";
const OWNERSHIP_BOUNDS_RULE: &str = "min_inclusive_max_exclusive";
const PLACEMENT_RULE: &str = "nested_power_of_two_global_lattice_dense_first";
const COLLAR_RULE: &str = "one_next_coarser_spacing_outside_dense_boundary";
const GRID_OVERLAP_RULE: &str = "finer_analysis_spacing_then_boundary_collar_then_stable_tile_id";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityBoundsMm {
    pub min: [i64; 2],
    pub max: [i64; 2],
}

impl CityBoundsMm {
    #[must_use]
    pub const fn new(min: [i64; 2], max: [i64; 2]) -> Self {
        Self { min, max }
    }

    #[must_use]
    pub fn contains_closed(self, point: [i64; 2]) -> bool {
        (0..2).all(|axis| self.min[axis] <= point[axis] && point[axis] <= self.max[axis])
    }

    #[must_use]
    pub fn contains_min_inclusive_max_exclusive(self, point: [i64; 2]) -> bool {
        (0..2).all(|axis| self.min[axis] <= point[axis] && point[axis] < self.max[axis])
    }

    #[must_use]
    pub fn intersection(self, other: Self) -> Option<Self> {
        let intersection = Self {
            min: [self.min[0].max(other.min[0]), self.min[1].max(other.min[1])],
            max: [self.max[0].min(other.max[0]), self.max[1].min(other.max[1])],
        };
        (intersection.min[0] < intersection.max[0] && intersection.min[1] < intersection.max[1])
            .then_some(intersection)
    }

    fn validate(self, label: &str) -> Result<()> {
        if self.min[0] >= self.max[0] || self.min[1] >= self.max[1] {
            return invalid(format!("{label} must increase on both horizontal axes"));
        }
        Ok(())
    }

    fn expand(self, amount_mm: i64, label: &str) -> Result<Self> {
        let min_east = self.min[0]
            .checked_sub(amount_mm)
            .ok_or_else(|| invalid_error(format!("{label} east minimum overflows")))?;
        let min_north = self.min[1]
            .checked_sub(amount_mm)
            .ok_or_else(|| invalid_error(format!("{label} north minimum overflows")))?;
        let max_east = self.max[0]
            .checked_add(amount_mm)
            .ok_or_else(|| invalid_error(format!("{label} east maximum overflows")))?;
        let max_north = self.max[1]
            .checked_add(amount_mm)
            .ok_or_else(|| invalid_error(format!("{label} north maximum overflows")))?;
        Ok(Self::new([min_east, min_north], [max_east, max_north]))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerOwnership {
    Owned,
    Guard,
    Outside,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellProbeSlice {
    pub grid_index: CellGridIndex,
    pub local_to_city_enu_mm: [i64; 3],
    pub probe_footprint_m: u32,
    pub stride_m: u32,
    pub pairwise_overlap_m: u32,
    pub ownership_guard_m: u32,
    pub geometry_material_halo_m: u32,
    pub baked_path_horizon_m: u32,
    pub probe_footprint_bounds_city_enu_mm: CityBoundsMm,
    pub ownership_bounds_city_enu_mm: CityBoundsMm,
    pub geometry_material_bounds_city_enu_mm: CityBoundsMm,
    pub probe_bounds_rule: String,
    pub ownership_bounds_rule: String,
}

impl CellProbeSlice {
    pub fn for_grid_index(grid_index: CellGridIndex) -> Result<Self> {
        verify_world_constants()?;
        let center_east = i64::from(grid_index.east)
            .checked_mul(CELL_STRIDE_MM)
            .ok_or_else(|| invalid_error("cell east translation overflows"))?;
        let center_north = i64::from(grid_index.north)
            .checked_mul(CELL_STRIDE_MM)
            .ok_or_else(|| invalid_error("cell north translation overflows"))?;
        let footprint_half = CELL_PROBE_FOOTPRINT_MM / 2;
        let ownership_half = CELL_STRIDE_MM / 2;
        let footprint = CityBoundsMm::new(
            [center_east - footprint_half, center_north - footprint_half],
            [center_east + footprint_half, center_north + footprint_half],
        );
        let ownership = CityBoundsMm::new(
            [center_east - ownership_half, center_north - ownership_half],
            [center_east + ownership_half, center_north + ownership_half],
        );
        let geometry_material = footprint.expand(CELL_GEOMETRY_HALO_MM, "geometry halo")?;
        Ok(Self {
            grid_index,
            local_to_city_enu_mm: [center_east, center_north, 0],
            probe_footprint_m: 585,
            stride_m: 485,
            pairwise_overlap_m: 100,
            ownership_guard_m: 50,
            geometry_material_halo_m: 600,
            baked_path_horizon_m: 600,
            probe_footprint_bounds_city_enu_mm: footprint,
            ownership_bounds_city_enu_mm: ownership,
            geometry_material_bounds_city_enu_mm: geometry_material,
            probe_bounds_rule: CELL_BOUNDS_RULE.to_owned(),
            ownership_bounds_rule: OWNERSHIP_BOUNDS_RULE.to_owned(),
        })
    }

    #[must_use]
    pub fn classify_listener_city_enu_mm(&self, point: [i64; 2]) -> ListenerOwnership {
        if !self
            .probe_footprint_bounds_city_enu_mm
            .contains_closed(point)
        {
            ListenerOwnership::Outside
        } else if self
            .ownership_bounds_city_enu_mm
            .contains_min_inclusive_max_exclusive(point)
        {
            ListenerOwnership::Owned
        } else {
            ListenerOwnership::Guard
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AboveMaximumLayerPolicy {
    DirectReflectionsOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkyPathingPolicy {
    pub maximum_layer_m: u32,
    pub above: AboveMaximumLayerPolicy,
}

impl Default for SkyPathingPolicy {
    fn default() -> Self {
        Self {
            maximum_layer_m: MAXIMUM_PROBE_LAYER_M,
            above: AboveMaximumLayerPolicy::DirectReflectionsOnly,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElevatedProbeLayerPolicy {
    pub id: String,
    pub up_mm: i64,
    pub spacing_m: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeTierPolicy {
    pub id: String,
    pub ground_up_mm: i64,
    pub ground_spacing_m: u32,
    pub analysis_spacing_m: u32,
    pub regions_city_enu_mm: Vec<CityBoundsMm>,
    pub elevated_layers: Vec<ElevatedProbeLayerPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GradedProbePolicy {
    pub lattice_origin_city_enu_mm: [i64; 2],
    pub tiers: Vec<ProbeTierPolicy>,
    pub sky_pathing_policy: SkyPathingPolicy,
}

impl GradedProbePolicy {
    #[must_use]
    pub fn new(lattice_origin_city_enu_mm: [i64; 2], tiers: Vec<ProbeTierPolicy>) -> Self {
        Self {
            lattice_origin_city_enu_mm,
            tiers,
            sky_pathing_policy: SkyPathingPolicy::default(),
        }
    }

    pub fn plan_cell(&self, grid_index: CellGridIndex) -> Result<CityBakeV2ProbePlan> {
        let cell = CellProbeSlice::for_grid_index(grid_index)?;
        let layout = self.resolve_bounds(cell.probe_footprint_bounds_city_enu_mm)?;
        let byte_estimate = estimate_bytes(&layout.probes)?;
        let grid_set = build_grid_set(&layout.policy, &cell, &layout.probe_layout_sha256)?;
        let plan = CityBakeV2ProbePlan {
            schema_version: CITY_BAKE_V2_SCHEMA_ID.to_owned(),
            artifact_state: CityBakeV2ArtifactState::ProbePlan,
            coordinate_frame: COORDINATE_FRAME.to_owned(),
            coordinate_encoding: COORDINATE_ENCODING.to_owned(),
            placement_rule: PLACEMENT_RULE.to_owned(),
            placement_policy_sha256: layout.placement_policy_sha256,
            probe_layout_sha256: layout.probe_layout_sha256,
            cell,
            policy: layout.policy,
            tier_summaries: layout.tier_summaries,
            grid_set,
            probes: layout.probes,
            byte_estimate,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Resolves the exact global-lattice probe sequence inside arbitrary city
    /// ENU bounds without assigning those bounds mobile-cell semantics.
    ///
    /// Desktop oracle tooling uses this path so a 1.17 km monolith cannot be
    /// mislabeled as a 585 m phone cell or inherit the phone's 600 m path cap.
    pub fn resolve_bounds(
        &self,
        bounds_city_enu_mm: CityBoundsMm,
    ) -> Result<ResolvedGradedProbeLayout> {
        bounds_city_enu_mm.validate("graded probe layout bounds")?;
        let policy = canonical_policy(self)?;
        validate_boundary_transitions(&policy, bounds_city_enu_mm)?;
        let policy_bytes = serde_json::to_vec(&policy).map_err(|error| {
            invalid_error(format!("serialize canonical graded probe policy: {error}"))
        })?;
        let placement_policy_sha256 = sha256_hex(&policy_bytes);
        let probes = enumerate_probes(&policy, bounds_city_enu_mm)?;
        if probes.is_empty() {
            return invalid("graded probe layout bounds contain no planned probes");
        }
        let probe_layout_sha256 = probe_layout_hash(&probes)?;
        let tier_summaries = summarize_tiers(&policy, &probes)?;
        Ok(ResolvedGradedProbeLayout {
            bounds_city_enu_mm,
            placement_policy_sha256,
            probe_layout_sha256,
            policy,
            tier_summaries,
            probes,
        })
    }
}

/// Canonical global graded layout resolved over caller-declared city bounds.
/// It is deliberately neutral about whether the caller is a mobile cell,
/// desktop oracle, or offline analysis tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedGradedProbeLayout {
    pub bounds_city_enu_mm: CityBoundsMm,
    pub placement_policy_sha256: String,
    pub probe_layout_sha256: String,
    pub policy: GradedProbePolicy,
    pub tier_summaries: Vec<ProbeTierSummary>,
    pub probes: Vec<ProbeSite>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CityBakeV2ArtifactState {
    ProbePlan,
    BakedProbeBatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSite {
    pub center_city_enu_mm: [i64; 3],
    pub radius_mm: u32,
    pub global_lattice_index: [i64; 2],
    pub tier_id: String,
    pub layer_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeLayerSummary {
    pub id: String,
    pub up_mm: i64,
    pub spacing_m: u32,
    pub probe_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeTierSummary {
    pub id: String,
    pub ground_spacing_m: u32,
    pub analysis_spacing_m: u32,
    pub layers: Vec<ProbeLayerSummary>,
    pub probe_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisTileKind {
    Tier,
    BoundaryCollar,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisTile {
    pub id: String,
    pub kind: AnalysisTileKind,
    pub tier_id: String,
    pub coarser_tier_id: Option<String>,
    pub bounds_city_enu_mm: CityBoundsMm,
    pub local_probe_spacing_m: u32,
    pub analysis_spacing_m: u32,
    pub placement_layout_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisGridSet {
    pub overlap_rule: String,
    pub boundary_collar_rule: String,
    pub tiles: Vec<AnalysisTile>,
}

impl AnalysisGridSet {
    #[must_use]
    pub fn tile_for_city_enu_mm(&self, point: [i64; 2]) -> Option<&AnalysisTile> {
        self.tiles
            .iter()
            .filter(|tile| tile.bounds_city_enu_mm.contains_closed(point))
            .min_by_key(|tile| {
                (
                    tile.analysis_spacing_m,
                    match tile.kind {
                        AnalysisTileKind::BoundaryCollar => 0_u8,
                        AnalysisTileKind::Tier => 1_u8,
                    },
                    tile.local_probe_spacing_m,
                    tile.id.as_str(),
                )
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeByteEstimate {
    pub estimator_revision: String,
    pub path_horizon_m: u32,
    pub probe_count: u64,
    pub reachable_ordered_pair_count: u64,
    pub estimated_raw_bytes: u64,
    pub projected_low_bytes: u64,
    pub projected_high_bytes: u64,
    pub target_raw_probe_payload_bytes: u64,
    pub hard_raw_probe_payload_bytes: u64,
    pub estimate_within_target: bool,
    pub projected_high_within_hard_limit: bool,
    /// Optional additive model data.  `None` is the legacy Q×10 envelope and
    /// is intentionally omitted from JSON for byte compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibrated_model: Option<CalibratedProbeByteModelV2>,
}

/// Deterministic, package-derived geometry observable for the additive mobile
/// estimator.  Coordinates are quantized from the exact package mesh and
/// probe plan to signed integer millimetres before any intersection test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshOpenPairObservable {
    pub algorithm: String,
    pub coordinate_encoding: String,
    /// Bound by the CLI once the package manifest is available.  The pure
    /// geometry helper leaves this absent so it can be used independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_sha256: Option<String>,
    pub owner_ground_height_mm: i64,
    pub owner_ground_probe_count: u64,
    pub wall_edge_count: u64,
    pub blocked_owner_ground_ordered_pair_count: u64,
    pub open_owner_ground_ordered_pair_count: u64,
}

/// Versioned data for the calibrated additive estimator.  The coefficients are
/// integer constants from the canonical v6 report; all counts are recomputed
/// from the exact plan and package mesh before this value is admitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibratedProbeByteModelV2 {
    pub revision: String,
    pub fixed_bytes: u64,
    pub owner_home_bytes_per_probe: u64,
    pub route_core_bytes_per_probe: u64,
    pub transition_bytes_per_probe: u64,
    pub residual_bytes_per_probe: u64,
    pub open_owner_ground_ordered_pair_bytes: u64,
    pub owner_home_probe_count: u64,
    pub route_core_probe_count: u64,
    pub transition_probe_count: u64,
    pub residual_probe_count: u64,
    pub owner_ground_probe_count: u64,
    pub open_owner_ground_ordered_pair_count: u64,
    pub point_estimate_bytes: u64,
    pub projected_low_bytes: u64,
    pub projected_high_bytes: u64,
    pub reservation_bytes: u64,
    pub mesh_observable: MeshOpenPairObservable,
}

impl CalibratedProbeByteModelV2 {
    pub fn validate(&self) -> Result<()> {
        if self.revision != FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION
            || self.fixed_bytes != 6_000
            || self.owner_home_bytes_per_probe != 3_315
            || self.route_core_bytes_per_probe != 31
            || self.transition_bytes_per_probe != 54
            || self.residual_bytes_per_probe != 30
            || self.open_owner_ground_ordered_pair_bytes != 14
            || self.owner_ground_probe_count != self.owner_home_probe_count
            || self.mesh_observable.owner_ground_probe_count != self.owner_ground_probe_count
            || self.mesh_observable.owner_ground_height_mm != 1_500
            || self.mesh_observable.open_owner_ground_ordered_pair_count
                != self.open_owner_ground_ordered_pair_count
            || self.mesh_observable.algorithm != FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM
            || self.mesh_observable.coordinate_encoding
                != FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING
            || self
                .mesh_observable
                .mesh_sha256
                .as_ref()
                .is_some_and(|hash| {
                    hash.len() != 64
                        || !hash
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                })
        {
            return invalid("calibrated estimator model identity or observable is invalid");
        }
        let total_pairs = self
            .owner_ground_probe_count
            .checked_mul(self.owner_ground_probe_count.saturating_sub(1))
            .ok_or_else(|| invalid_error("calibrated owner ordered pair count overflows"))?;
        if self
            .mesh_observable
            .blocked_owner_ground_ordered_pair_count
            .checked_add(self.open_owner_ground_ordered_pair_count)
            != Some(total_pairs)
        {
            return invalid("calibrated mesh observable pair accounting is invalid");
        }
        let point = self
            .fixed_bytes
            .checked_add(
                self.owner_home_probe_count
                    .checked_mul(self.owner_home_bytes_per_probe)
                    .ok_or_else(|| invalid_error("calibrated owner tier bytes overflow"))?,
            )
            .and_then(|bytes| {
                bytes.checked_add(
                    self.route_core_probe_count
                        .checked_mul(self.route_core_bytes_per_probe)?,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    self.transition_probe_count
                        .checked_mul(self.transition_bytes_per_probe)?,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    self.residual_probe_count
                        .checked_mul(self.residual_bytes_per_probe)?,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    self.open_owner_ground_ordered_pair_count
                        .checked_mul(self.open_owner_ground_ordered_pair_bytes)?,
                )
            })
            .ok_or_else(|| invalid_error("calibrated estimator point bytes overflow"))?;
        if self.point_estimate_bytes != point {
            return invalid("calibrated estimator point bytes do not match its additive formula");
        }
        let low = point
            .checked_mul(70)
            .ok_or_else(|| invalid_error("calibrated estimator lower band overflows"))?
            / 100;
        let high = point
            .checked_mul(130)
            .and_then(|value| value.checked_add(99))
            .ok_or_else(|| invalid_error("calibrated estimator upper band overflows"))?
            / 100;
        if self.projected_low_bytes != low
            || self.projected_high_bytes != high
            || self.reservation_bytes != high
        {
            return invalid("calibrated estimator band or reservation is not exact");
        }
        Ok(())
    }
}

/// Compute the frozen mesh-open-pair observable.  `local_to_city_enu_mm`
/// translates plan centres into the package's local ENU frame; it is part of
/// the package/plan binding and is never inferred from floating point values.
pub fn mesh_open_pair_observable(
    mesh: &AcousticMesh,
    probes: &[ProbeSite],
    local_to_city_enu_mm: [i64; 3],
) -> Result<MeshOpenPairObservable> {
    let owner_probes = probes
        .iter()
        .filter(|probe| probe.tier_id == "owner-home" && probe.layer_id == "ground")
        .collect::<Vec<_>>();
    // A route/residual cell may legitimately have no owner-home probes.  The
    // strict policy still fixes the sole ground layer height at 1,500 mm so
    // its zero-pair observable remains deterministic and verifiable.
    let owner_ground_height_mm = owner_probes
        .first()
        .map(|probe| probe.center_city_enu_mm[2])
        .unwrap_or(1_500);
    if owner_probes
        .iter()
        .any(|probe| probe.center_city_enu_mm[2] != owner_ground_height_mm)
        || owner_ground_height_mm <= 0
    {
        return invalid("owner-home ground probes must have one positive signed-mm height");
    }

    let mut wall_edges = BTreeSet::<([i64; 2], [i64; 2])>::new();
    for triangle in &mesh.triangles {
        let mut vertices = [EnuVector3::default(); 3];
        for (destination, source) in vertices.iter_mut().zip(triangle) {
            *destination = mesh
                .vertices_enu_m
                .get(usize::try_from(*source).map_err(|_| {
                    invalid_error("mesh triangle index exceeds the supported usize range")
                })?)
                .copied()
                .ok_or_else(|| invalid_error("mesh triangle index is out of range"))?;
        }
        let mut z = [0_i64; 3];
        for (destination, vertex) in z.iter_mut().zip(&vertices) {
            *destination = quantize_mesh_mm(vertex.up_m, "mesh up coordinate")?;
        }
        if z.iter().min().copied().unwrap_or(0) <= owner_ground_height_mm
            && owner_ground_height_mm <= z.iter().max().copied().unwrap_or(0)
        {
            let mut xy = [[0_i64; 2]; 3];
            for (destination, vertex) in xy.iter_mut().zip(&vertices) {
                destination[0] = quantize_mesh_mm(vertex.east_m, "mesh east coordinate")?;
                destination[1] = quantize_mesh_mm(vertex.north_m, "mesh north coordinate")?;
            }
            for (left, right) in [(xy[0], xy[1]), (xy[1], xy[2]), (xy[2], xy[0])] {
                if left != right {
                    let edge = if left <= right {
                        (left, right)
                    } else {
                        (right, left)
                    };
                    wall_edges.insert(edge);
                }
            }
        }
    }

    let mut blocked = 0_u64;
    let mut open = 0_u64;
    for (index, left) in owner_probes.iter().enumerate() {
        let left = [
            left.center_city_enu_mm[0]
                .checked_sub(local_to_city_enu_mm[0])
                .ok_or_else(|| invalid_error("owner probe east coordinate overflows"))?,
            left.center_city_enu_mm[1]
                .checked_sub(local_to_city_enu_mm[1])
                .ok_or_else(|| invalid_error("owner probe north coordinate overflows"))?,
        ];
        for right_probe in &owner_probes[index + 1..] {
            let right = [
                right_probe.center_city_enu_mm[0]
                    .checked_sub(local_to_city_enu_mm[0])
                    .ok_or_else(|| invalid_error("owner probe east coordinate overflows"))?,
                right_probe.center_city_enu_mm[1]
                    .checked_sub(local_to_city_enu_mm[1])
                    .ok_or_else(|| invalid_error("owner probe north coordinate overflows"))?,
            ];
            let mut is_blocked = false;
            for (edge_left, edge_right) in &wall_edges {
                if closed_segments_intersect(left, right, *edge_left, *edge_right)? {
                    is_blocked = true;
                    break;
                }
            }
            // Each unordered pair stands in for both ordered orientations
            // ((i, j) and (j, i)): the translated endpoints are identical and
            // segment intersection is symmetric under endpoint swap, so one
            // evaluation contributes exactly two identical ordered outcomes.
            if is_blocked {
                blocked = blocked
                    .checked_add(2)
                    .ok_or_else(|| invalid_error("blocked owner pair count overflows"))?;
            } else {
                open = open
                    .checked_add(2)
                    .ok_or_else(|| invalid_error("open owner pair count overflows"))?;
            }
        }
    }
    let owner_count = u64::try_from(owner_probes.len())
        .map_err(|_| invalid_error("owner-home probe count exceeds u64"))?;
    let expected_pairs = owner_count
        .checked_mul(owner_count.saturating_sub(1))
        .ok_or_else(|| invalid_error("owner ordered pair count overflows"))?;
    if blocked.checked_add(open) != Some(expected_pairs) {
        return invalid(
            "mesh open-pair accounting does not cover all ordered distinct owner pairs",
        );
    }
    Ok(MeshOpenPairObservable {
        algorithm: FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM.to_owned(),
        coordinate_encoding: FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING.to_owned(),
        mesh_sha256: None,
        owner_ground_height_mm,
        owner_ground_probe_count: owner_count,
        wall_edge_count: u64::try_from(wall_edges.len())
            .map_err(|_| invalid_error("wall edge count exceeds u64"))?,
        blocked_owner_ground_ordered_pair_count: blocked,
        open_owner_ground_ordered_pair_count: open,
    })
}

fn quantize_mesh_mm(value_m: f32, field: &str) -> Result<i64> {
    if !value_m.is_finite() {
        return invalid(format!("{field} must be finite"));
    }
    let value_mm = f64::from(value_m) * 1_000.0;
    if !value_mm.is_finite() || value_mm < i64::MIN as f64 || value_mm > i64::MAX as f64 {
        return invalid(format!("{field} overflows signed integer millimetres"));
    }
    Ok(value_mm.round() as i64)
}

fn orientation(a: [i64; 2], b: [i64; 2], c: [i64; 2]) -> Result<i128> {
    let ab_x = i128::from(b[0]) - i128::from(a[0]);
    let ab_y = i128::from(b[1]) - i128::from(a[1]);
    let ac_x = i128::from(c[0]) - i128::from(a[0]);
    let ac_y = i128::from(c[1]) - i128::from(a[1]);
    ab_x.checked_mul(ac_y)
        .and_then(|left| {
            ab_y.checked_mul(ac_x)
                .and_then(|right| left.checked_sub(right))
        })
        .ok_or_else(|| invalid_error("signed i128 segment orientation overflows"))
}

fn point_on_segment(a: [i64; 2], b: [i64; 2], point: [i64; 2]) -> Result<bool> {
    Ok(orientation(a, b, point)? == 0
        && point[0] >= a[0].min(b[0])
        && point[0] <= a[0].max(b[0])
        && point[1] >= a[1].min(b[1])
        && point[1] <= a[1].max(b[1]))
}

fn closed_segments_intersect(a: [i64; 2], b: [i64; 2], c: [i64; 2], d: [i64; 2]) -> Result<bool> {
    let first = [orientation(a, b, c)?, orientation(a, b, d)?];
    let second = [orientation(c, d, a)?, orientation(c, d, b)?];
    if (first[0] == 0 && point_on_segment(a, b, c)?)
        || (first[1] == 0 && point_on_segment(a, b, d)?)
        || (second[0] == 0 && point_on_segment(c, d, a)?)
        || (second[1] == 0 && point_on_segment(c, d, b)?)
    {
        return Ok(true);
    }
    Ok((first[0] > 0) != (first[1] > 0) && (second[0] > 0) != (second[1] > 0))
}

/// Recompute the additive model from the canonical plan and package mesh.
/// The four named tiers and their production spacings are deliberately fixed;
/// an arbitrary policy is not silently assigned calibrated coefficients.
pub fn calibrated_probe_byte_model(
    plan: &CityBakeV2ProbePlan,
    mesh: &AcousticMesh,
) -> Result<CalibratedProbeByteModelV2> {
    let expected = [
        ("owner-home", 4_u32),
        ("route-core", 8),
        ("transition", 16),
        ("residual", 32),
    ];
    if plan.placement_policy_sha256 != CALIBRATED_MOBILE_PLACEMENT_POLICY_SHA256 {
        return invalid(
            "calibrated estimator requires the exact Wave 17 resolved placement policy SHA-256",
        );
    }
    if plan.tier_summaries.len() != expected.len()
        || plan.policy.tiers.len() != expected.len()
        || plan
            .tier_summaries
            .iter()
            .zip(expected)
            .any(|(tier, (id, spacing))| tier.id != id || tier.ground_spacing_m != spacing)
        || plan
            .policy
            .tiers
            .iter()
            .zip(expected)
            .any(|(tier, (id, spacing))| tier.id != id || tier.ground_spacing_m != spacing)
    {
        return invalid(
            "calibrated estimator requires the canonical owner-home/route-core/transition/residual tier structure",
        );
    }
    let owner = &plan.tier_summaries[0];
    if owner.layers.len() != 1
        || owner.layers[0].id != "ground"
        || owner.layers[0].probe_count != owner.probe_count
        || owner.layers[0].up_mm <= 0
        || plan
            .probes
            .iter()
            .filter(|probe| probe.tier_id == "owner-home" && probe.layer_id == "ground")
            .count() as u64
            != owner.probe_count
    {
        return invalid("calibrated estimator owner-home tier must contain only one ground layer");
    }
    let observable = mesh_open_pair_observable(mesh, &plan.probes, plan.cell.local_to_city_enu_mm)?;
    if observable.owner_ground_height_mm != owner.layers[0].up_mm
        || observable.owner_ground_probe_count != owner.probe_count
    {
        return invalid(
            "calibrated estimator mesh observable does not bind owner-home ground tier",
        );
    }
    let mut model = CalibratedProbeByteModelV2 {
        revision: FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION.to_owned(),
        fixed_bytes: 6_000,
        owner_home_bytes_per_probe: 3_315,
        route_core_bytes_per_probe: 31,
        transition_bytes_per_probe: 54,
        residual_bytes_per_probe: 30,
        open_owner_ground_ordered_pair_bytes: 14,
        owner_home_probe_count: owner.probe_count,
        route_core_probe_count: plan.tier_summaries[1].probe_count,
        transition_probe_count: plan.tier_summaries[2].probe_count,
        residual_probe_count: plan.tier_summaries[3].probe_count,
        owner_ground_probe_count: observable.owner_ground_probe_count,
        open_owner_ground_ordered_pair_count: observable.open_owner_ground_ordered_pair_count,
        point_estimate_bytes: 0,
        projected_low_bytes: 0,
        projected_high_bytes: 0,
        reservation_bytes: 0,
        mesh_observable: observable,
    };
    let point = model
        .fixed_bytes
        .checked_add(
            model
                .owner_home_probe_count
                .checked_mul(model.owner_home_bytes_per_probe)
                .ok_or_else(|| invalid_error("calibrated owner tier bytes overflow"))?,
        )
        .and_then(|v| {
            v.checked_add(
                model
                    .route_core_probe_count
                    .checked_mul(model.route_core_bytes_per_probe)?,
            )
        })
        .and_then(|v| {
            v.checked_add(
                model
                    .transition_probe_count
                    .checked_mul(model.transition_bytes_per_probe)?,
            )
        })
        .and_then(|v| {
            v.checked_add(
                model
                    .residual_probe_count
                    .checked_mul(model.residual_bytes_per_probe)?,
            )
        })
        .and_then(|v| {
            v.checked_add(
                model
                    .open_owner_ground_ordered_pair_count
                    .checked_mul(model.open_owner_ground_ordered_pair_bytes)?,
            )
        })
        .ok_or_else(|| invalid_error("calibrated estimator point bytes overflow"))?;
    model.point_estimate_bytes = point;
    model.projected_low_bytes = point
        .checked_mul(70)
        .ok_or_else(|| invalid_error("calibrated estimator lower band overflows"))?
        / 100;
    model.projected_high_bytes = point
        .checked_mul(130)
        .and_then(|v| v.checked_add(99))
        .ok_or_else(|| invalid_error("calibrated estimator upper band overflows"))?
        / 100;
    model.reservation_bytes = model.projected_high_bytes;
    model.validate()?;
    Ok(model)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityBakeV2ProbePlan {
    pub schema_version: String,
    pub artifact_state: CityBakeV2ArtifactState,
    pub coordinate_frame: String,
    pub coordinate_encoding: String,
    pub placement_rule: String,
    pub placement_policy_sha256: String,
    pub probe_layout_sha256: String,
    pub cell: CellProbeSlice,
    pub policy: GradedProbePolicy,
    pub tier_summaries: Vec<ProbeTierSummary>,
    pub grid_set: AnalysisGridSet,
    pub probes: Vec<ProbeSite>,
    pub byte_estimate: ProbeByteEstimate,
}

impl CityBakeV2ProbePlan {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CITY_BAKE_V2_SCHEMA_ID
            || self.artifact_state != CityBakeV2ArtifactState::ProbePlan
            || self.coordinate_frame != COORDINATE_FRAME
            || self.coordinate_encoding != COORDINATE_ENCODING
            || self.placement_rule != PLACEMENT_RULE
        {
            return invalid("city-bake v2 probe-plan identity differs from the contract");
        }
        let canonical = canonical_policy(&self.policy)?;
        if canonical != self.policy {
            return invalid("city-bake v2 probe policy is not in canonical order");
        }
        let policy_bytes = serde_json::to_vec(&canonical).map_err(|error| {
            invalid_error(format!("serialize canonical graded probe policy: {error}"))
        })?;
        if sha256_hex(&policy_bytes) != self.placement_policy_sha256 {
            return invalid("graded probe placement-policy hash does not match its policy");
        }
        if self.cell != CellProbeSlice::for_grid_index(self.cell.grid_index)? {
            return invalid("graded probe cell slice differs from the mobile-cell contract");
        }
        let expected_probes =
            enumerate_probes(&canonical, self.cell.probe_footprint_bounds_city_enu_mm)?;
        if expected_probes != self.probes {
            return invalid("graded probe list differs from the resolved global lattice slice");
        }
        if probe_layout_hash(&self.probes)? != self.probe_layout_sha256 {
            return invalid("graded probe layout hash does not match its ordered probes");
        }
        if summarize_tiers(&canonical, &self.probes)? != self.tier_summaries {
            return invalid("graded probe tier summaries differ from the ordered probes");
        }
        if estimate_bytes(&self.probes)? != self.byte_estimate {
            return invalid("graded probe byte estimate differs from the ordered probes");
        }
        if build_grid_set(&canonical, &self.cell, &self.probe_layout_sha256)? != self.grid_set {
            return invalid("graded analysis GridSet differs from its policy and layout hash");
        }
        Ok(())
    }

    pub fn to_sidecar_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec_pretty(self)
            .map_err(|error| invalid_error(format!("serialize city-bake v2 probe plan: {error}")))
    }

    pub fn from_sidecar_bytes(bytes: &[u8]) -> Result<Self> {
        let plan: Self = serde_json::from_slice(bytes)
            .map_err(|error| invalid_error(format!("city-bake v2 probe-plan JSON: {error}")))?;
        plan.validate()?;
        Ok(plan)
    }

    #[must_use]
    pub fn local_probe_centres_m(&self) -> Vec<([f32; 3], f32)> {
        let translation = self.cell.local_to_city_enu_mm;
        self.probes
            .iter()
            .map(|probe| {
                let centre = [
                    (probe.center_city_enu_mm[0] - translation[0]) as f32 / 1_000.0,
                    (probe.center_city_enu_mm[1] - translation[1]) as f32 / 1_000.0,
                    (probe.center_city_enu_mm[2] - translation[2]) as f32 / 1_000.0,
                ];
                (centre, probe.radius_mm as f32 / 1_000.0)
            })
            .collect()
    }
}

/// Exact path-baker settings bound to a completed explicit probe artifact.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityBakeV2PathBakeSettings {
    pub num_visibility_samples: i32,
    pub probe_visibility_radius_m: f32,
    pub visibility_threshold: f32,
    pub visibility_range_m: f32,
    pub path_range_m: f32,
    pub num_threads: i32,
}

/// SDK and content identity of the opaque probe-batch payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityBakeV2ProbeBatchIdentity {
    pub capability: String,
    pub payload_path: String,
    pub compression: crate::PackageCompression,
    pub metadata_schema: String,
    pub steam_audio_version: String,
    pub upstream_commit: String,
    pub probe_count: u64,
    pub path_data_size_bytes: u64,
    pub serialized_size_bytes: u64,
    pub serialized_sha256: String,
}

/// Repeatable, timestamp-free telemetry emitted by the explicit bake path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityBakeV2DeterministicTelemetry {
    pub baker_revision: String,
    pub submitted_explicit_probe_count: u64,
    pub committed_probe_count: u64,
    pub insertion_order: String,
    pub bake_progress_callback_count: u32,
    pub final_bake_progress_millionths: u32,
}

/// Successfully baked form of `fightbox.city-bake.v2`.
///
/// The original plan is not mutated. This sidecar is created alongside the
/// opaque probe payload only after the SDK bake, serialization, and payload
/// validation succeed, and cross-binds that payload back to the exact plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityBakeV2BakedArtifact {
    pub schema_version: String,
    pub artifact_state: CityBakeV2ArtifactState,
    pub coordinate_frame: String,
    pub coordinate_encoding: String,
    pub placement_rule: String,
    pub probe_plan_content_sha256: String,
    pub placement_policy_sha256: String,
    pub probe_layout_sha256: String,
    pub estimator_revision: String,
    /// Present only for an explicitly selected additive estimator revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibrated_model: Option<CalibratedProbeByteModelV2>,
    pub city_id: String,
    pub cell_id: String,
    pub cell_grid_index: CellGridIndex,
    pub tier_summaries: Vec<ProbeTierSummary>,
    pub sky_pathing_policy: SkyPathingPolicy,
    pub planned_probe_count: u64,
    pub estimated_raw_bytes: u64,
    pub projected_low_bytes: u64,
    pub projected_high_bytes: u64,
    pub package_mesh_sha256: String,
    pub package_materials_sha256: String,
    pub pathing: CityBakeV2PathBakeSettings,
    pub probe_batch: CityBakeV2ProbeBatchIdentity,
    pub telemetry: CityBakeV2DeterministicTelemetry,
}

impl CityBakeV2BakedArtifact {
    #[allow(clippy::too_many_arguments)]
    pub fn bind_successful_bake(
        plan: &CityBakeV2ProbePlan,
        city_id: impl Into<String>,
        cell_id: impl Into<String>,
        package_mesh_sha256: impl Into<String>,
        package_materials_sha256: impl Into<String>,
        pathing: CityBakeV2PathBakeSettings,
        probe_batch: CityBakeV2ProbeBatchIdentity,
        telemetry: CityBakeV2DeterministicTelemetry,
    ) -> Result<Self> {
        plan.validate()?;
        let probe_plan_content_sha256 = sha256_hex(&plan.to_sidecar_bytes()?);
        let artifact = Self {
            schema_version: CITY_BAKE_V2_SCHEMA_ID.to_owned(),
            artifact_state: CityBakeV2ArtifactState::BakedProbeBatch,
            coordinate_frame: plan.coordinate_frame.clone(),
            coordinate_encoding: plan.coordinate_encoding.clone(),
            placement_rule: plan.placement_rule.clone(),
            probe_plan_content_sha256,
            placement_policy_sha256: plan.placement_policy_sha256.clone(),
            probe_layout_sha256: plan.probe_layout_sha256.clone(),
            estimator_revision: plan.byte_estimate.estimator_revision.clone(),
            calibrated_model: None,
            city_id: city_id.into(),
            cell_id: cell_id.into(),
            cell_grid_index: plan.cell.grid_index,
            tier_summaries: plan.tier_summaries.clone(),
            sky_pathing_policy: plan.policy.sky_pathing_policy.clone(),
            planned_probe_count: plan.byte_estimate.probe_count,
            estimated_raw_bytes: plan.byte_estimate.estimated_raw_bytes,
            projected_low_bytes: plan.byte_estimate.projected_low_bytes,
            projected_high_bytes: plan.byte_estimate.projected_high_bytes,
            package_mesh_sha256: package_mesh_sha256.into(),
            package_materials_sha256: package_materials_sha256.into(),
            pathing,
            probe_batch,
            telemetry,
        };
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CITY_BAKE_V2_SCHEMA_ID
            || self.artifact_state != CityBakeV2ArtifactState::BakedProbeBatch
            || self.coordinate_frame != COORDINATE_FRAME
            || self.coordinate_encoding != COORDINATE_ENCODING
            || self.placement_rule != PLACEMENT_RULE
            || (self.estimator_revision != PROBE_LAYOUT_ESTIMATOR_REVISION
                && self.estimator_revision != FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION)
        {
            return invalid("completed city-bake v2 identity differs from the contract");
        }
        if self.estimator_revision == FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION {
            let Some(model) = &self.calibrated_model else {
                return invalid("calibrated completed bake is missing its model data");
            };
            model.validate()?;
            let expected_tiers = [
                ("owner-home", model.owner_home_probe_count),
                ("route-core", model.route_core_probe_count),
                ("transition", model.transition_probe_count),
                ("residual", model.residual_probe_count),
            ];
            if self.tier_summaries.len() != expected_tiers.len()
                || self
                    .tier_summaries
                    .iter()
                    .zip(expected_tiers)
                    .any(|(tier, (id, count))| tier.id != id || tier.probe_count != count)
                || self.planned_probe_count
                    != model
                        .owner_home_probe_count
                        .checked_add(model.route_core_probe_count)
                        .and_then(|total| total.checked_add(model.transition_probe_count))
                        .and_then(|total| total.checked_add(model.residual_probe_count))
                        .ok_or_else(|| invalid_error("calibrated model tier count overflows"))?
            {
                return invalid("calibrated model tier counts do not bind completed plan");
            }
            let owner_ground = self
                .tier_summaries
                .first()
                .and_then(|tier| tier.layers.iter().find(|layer| layer.id == "ground"));
            if owner_ground.map(|layer| layer.probe_count) != Some(model.owner_ground_probe_count) {
                return invalid("calibrated model owner-ground count does not bind completed plan");
            }
        } else if self.calibrated_model.is_some() {
            return invalid("legacy completed bake must not carry calibrated model data");
        }
        for (hash, label) in [
            (&self.probe_plan_content_sha256, "probe plan"),
            (&self.placement_policy_sha256, "placement policy"),
            (&self.probe_layout_sha256, "probe layout"),
            (&self.package_mesh_sha256, "package mesh"),
            (&self.package_materials_sha256, "package materials"),
            (
                &self.probe_batch.serialized_sha256,
                "serialized probe batch",
            ),
        ] {
            validate_sha256(hash, label)?;
        }
        if self.city_id.is_empty()
            || self.cell_id != crate::stable_cell_id(&self.city_id, self.cell_grid_index)
        {
            return invalid("completed city-bake v2 cell identity is not canonical");
        }
        if self.sky_pathing_policy != SkyPathingPolicy::default() {
            return invalid(
                "completed city-bake v2 must declare direct+reflections-only above 63 m",
            );
        }
        let summarized_probe_count = self
            .tier_summaries
            .iter()
            .try_fold(0_u64, |total, tier| total.checked_add(tier.probe_count))
            .ok_or_else(|| invalid_error("completed city-bake v2 tier count overflows"))?;
        if self.planned_probe_count == 0
            || summarized_probe_count != self.planned_probe_count
            || self.probe_batch.probe_count != self.planned_probe_count
            || self.telemetry.submitted_explicit_probe_count != self.planned_probe_count
            || self.telemetry.committed_probe_count != self.planned_probe_count
        {
            return invalid(
                "completed city-bake v2 planned, tier, submitted, committed, and serialized probe counts must agree",
            );
        }
        if self.probe_batch.capability != crate::STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY
            || self.probe_batch.payload_path != "probe-batch.bin"
            || self.probe_batch.compression != crate::PackageCompression::None
            || self.probe_batch.metadata_schema.is_empty()
            || self.probe_batch.steam_audio_version.is_empty()
            || self.probe_batch.upstream_commit.is_empty()
            || self.probe_batch.path_data_size_bytes == 0
            || self.probe_batch.serialized_size_bytes == 0
            || self.probe_batch.serialized_size_bytes > MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES
        {
            return invalid("completed city-bake v2 probe-batch identity is invalid");
        }
        if self.estimated_raw_bytes == 0
            || self.projected_low_bytes > self.estimated_raw_bytes
            || self.estimated_raw_bytes > self.projected_high_bytes
            || self.probe_batch.serialized_size_bytes > self.projected_high_bytes
        {
            return invalid("completed city-bake v2 exceeded or corrupted its byte estimate");
        }
        if self.pathing.num_visibility_samples <= 0
            || !self.pathing.probe_visibility_radius_m.is_finite()
            || self.pathing.probe_visibility_radius_m < 0.0
            || !self.pathing.visibility_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.pathing.visibility_threshold)
            || !self.pathing.visibility_range_m.is_finite()
            || self.pathing.visibility_range_m <= 0.0
            || self.pathing.path_range_m.to_bits() != (MOBILE_BAKED_PATH_HORIZON_M as f32).to_bits()
            || self.pathing.num_threads <= 0
        {
            return invalid("completed city-bake v2 path settings are invalid");
        }
        if self.telemetry.baker_revision.is_empty()
            || self.telemetry.insertion_order != "probe_plan_local_centres"
            || self.telemetry.final_bake_progress_millionths > 1_000_000
        {
            return invalid("completed city-bake v2 deterministic telemetry is invalid");
        }
        Ok(())
    }

    pub fn to_sidecar_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec_pretty(self).map_err(|error| {
            invalid_error(format!("serialize completed city-bake v2 sidecar: {error}"))
        })
    }

    pub fn from_sidecar_bytes(bytes: &[u8]) -> Result<Self> {
        let artifact: Self = serde_json::from_slice(bytes).map_err(|error| {
            invalid_error(format!("completed city-bake v2 sidecar JSON: {error}"))
        })?;
        artifact.validate()?;
        Ok(artifact)
    }
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
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

/// Writes the deterministic plan sidecar before [`crate::write_package_v2`]
/// indexes it. The required capability remains explicitly in `probe_plan`
/// state; a completed Steam Audio batch must be bound by the later bake lane.
pub fn write_city_bake_v2_plan_sidecar(
    package_directory: impl AsRef<Path>,
    plan: &CityBakeV2ProbePlan,
) -> Result<CapabilityExtension> {
    let bytes = plan.to_sidecar_bytes()?;
    let extension = CapabilityExtension::uncompressed(
        CITY_BAKE_V2_CAPABILITY,
        ExtensionRequirement::Required,
        CITY_BAKE_V2_SIDECAR_PATH,
        &bytes,
    );
    extension.validate()?;
    let path = package_directory.as_ref().join(CITY_BAKE_V2_SIDECAR_PATH);
    let parent = path
        .parent()
        .ok_or_else(|| invalid_error("city-bake v2 sidecar path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| WorldError::io(parent, error))?;
    fs::write(&path, bytes).map_err(|error| WorldError::io(path, error))?;
    Ok(extension)
}

fn canonical_policy(policy: &GradedProbePolicy) -> Result<GradedProbePolicy> {
    verify_world_constants()?;
    if policy.sky_pathing_policy != SkyPathingPolicy::default() {
        return invalid("sky pathing must declare maximum layer 63 m and direct+reflections above");
    }
    if policy.tiers.is_empty() || policy.tiers.len() > 64 {
        return invalid("graded probe policy must contain 1..=64 tiers");
    }
    let mut policy = policy.clone();
    let mut tier_ids = BTreeSet::new();
    let mut all_spacings = BTreeSet::new();
    let mut ground_spacings = BTreeSet::new();
    let mut has_maximum_layer = false;
    for tier in &mut policy.tiers {
        validate_stable_id(&tier.id, "probe tier")?;
        if !tier_ids.insert(tier.id.clone()) {
            return invalid(format!("duplicate probe tier ID {:?}", tier.id));
        }
        validate_spacing(tier.ground_spacing_m, "ground probe")?;
        validate_analysis_spacing(tier.analysis_spacing_m)?;
        all_spacings.insert(tier.ground_spacing_m);
        ground_spacings.insert(tier.ground_spacing_m);
        if tier.ground_up_mm > i64::from(MAXIMUM_PROBE_LAYER_M) * MILLIMETRES_PER_METRE {
            return invalid(format!(
                "tier {:?} ground layer exceeds the declared 63 m maximum",
                tier.id
            ));
        }
        if tier.regions_city_enu_mm.is_empty() || tier.regions_city_enu_mm.len() > 1_024 {
            return invalid(format!("tier {:?} must contain 1..=1024 regions", tier.id));
        }
        tier.regions_city_enu_mm.sort();
        for region in &tier.regions_city_enu_mm {
            region.validate(&format!("tier {:?} region", tier.id))?;
        }
        if tier
            .regions_city_enu_mm
            .windows(2)
            .any(|regions| regions[0] == regions[1])
        {
            return invalid(format!("tier {:?} contains a duplicate region", tier.id));
        }
        if tier.elevated_layers.len() > 16 {
            return invalid(format!(
                "tier {:?} contains more than 16 elevated layers",
                tier.id
            ));
        }
        tier.elevated_layers
            .sort_by(|left, right| (left.up_mm, &left.id).cmp(&(right.up_mm, &right.id)));
        let mut layer_ids = BTreeSet::new();
        let mut layer_heights = BTreeSet::new();
        for layer in &tier.elevated_layers {
            validate_stable_id(&layer.id, "probe layer")?;
            validate_spacing(layer.spacing_m, "elevated probe")?;
            all_spacings.insert(layer.spacing_m);
            if !layer_ids.insert(layer.id.as_str()) || !layer_heights.insert(layer.up_mm) {
                return invalid(format!(
                    "tier {:?} elevated layer IDs and heights must be unique",
                    tier.id
                ));
            }
            if layer.up_mm <= tier.ground_up_mm
                || layer.up_mm > i64::from(MAXIMUM_PROBE_LAYER_M) * MILLIMETRES_PER_METRE
            {
                return invalid(format!(
                    "tier {:?} elevated layers must be above ground and at or below 63 m",
                    tier.id
                ));
            }
            has_maximum_layer |=
                layer.up_mm == i64::from(MAXIMUM_PROBE_LAYER_M) * MILLIMETRES_PER_METRE;
        }
    }
    if !has_maximum_layer {
        return invalid("graded production policy must include at least one 63 m probe layer");
    }
    let minimum_spacing = all_spacings
        .iter()
        .next()
        .copied()
        .ok_or_else(|| invalid_error("graded probe policy contains no spacings"))?;
    for spacing in &all_spacings {
        if spacing % minimum_spacing != 0 || !(spacing / minimum_spacing).is_power_of_two() {
            return invalid(
                "all probe spacings must be power-of-two multiples on the global lattice",
            );
        }
    }
    if ground_spacings.contains(&8)
        && ground_spacings.contains(&32)
        && !ground_spacings.contains(&16)
    {
        return invalid("an 8 m to 32 m policy must include the 16 m transition spacing");
    }
    policy.tiers.sort_by(|left, right| {
        (left.ground_spacing_m, &left.id).cmp(&(right.ground_spacing_m, &right.id))
    });
    Ok(policy)
}

fn validate_spacing(spacing_m: u32, label: &str) -> Result<()> {
    if !matches!(spacing_m, 4 | 8 | 16 | 32) {
        return invalid(format!(
            "{label} spacing must be one of the production tiers 4, 8, 16, or 32 m"
        ));
    }
    Ok(())
}

fn validate_analysis_spacing(spacing_m: u32) -> Result<()> {
    if spacing_m == 0 || spacing_m > 256 || !spacing_m.is_power_of_two() {
        return invalid("analysis spacing must be a power of two in 1..=256 m");
    }
    Ok(())
}

fn validate_stable_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || !value.as_bytes()[0].is_ascii_lowercase()
    {
        return invalid(format!(
            "{label} ID must be a lowercase stable identifier starting with a letter"
        ));
    }
    Ok(())
}

fn validate_boundary_transitions(
    policy: &GradedProbePolicy,
    bounds_city_enu_mm: CityBoundsMm,
) -> Result<()> {
    for tier in &policy.tiers {
        let Some(next_spacing) = policy
            .tiers
            .iter()
            .map(|candidate| candidate.ground_spacing_m)
            .filter(|spacing| *spacing > tier.ground_spacing_m)
            .min()
        else {
            continue;
        };
        if next_spacing > tier.ground_spacing_m.saturating_mul(2) {
            return invalid(format!(
                "tier {:?} transitions directly from {} m to {} m without the intermediate tier",
                tier.id, tier.ground_spacing_m, next_spacing
            ));
        }
        let width_mm = i64::from(next_spacing)
            .checked_mul(MILLIMETRES_PER_METRE)
            .ok_or_else(|| invalid_error("probe transition collar width overflows"))?;
        for region in &tier.regions_city_enu_mm {
            for strip in boundary_collar_strips(*region, width_mm)? {
                let Some(strip) = strip.intersection(bounds_city_enu_mm) else {
                    continue;
                };
                validate_transition_strip(policy, tier, next_spacing, strip)?;
            }
        }
    }
    Ok(())
}

fn validate_transition_strip(
    policy: &GradedProbePolicy,
    tier: &ProbeTierPolicy,
    next_spacing: u32,
    strip: CityBoundsMm,
) -> Result<()> {
    let mut east_edges = vec![strip.min[0], strip.max[0]];
    let mut north_edges = vec![strip.min[1], strip.max[1]];
    for candidate in &policy.tiers {
        for region in &candidate.regions_city_enu_mm {
            if let Some(overlap) = strip.intersection(*region) {
                east_edges.push(overlap.min[0]);
                east_edges.push(overlap.max[0]);
                north_edges.push(overlap.min[1]);
                north_edges.push(overlap.max[1]);
            }
        }
    }
    east_edges.sort_unstable();
    east_edges.dedup();
    north_edges.sort_unstable();
    north_edges.dedup();
    let partition_count = east_edges
        .len()
        .saturating_sub(1)
        .checked_mul(north_edges.len().saturating_sub(1))
        .ok_or_else(|| invalid_error("probe transition partition count overflows"))?;
    if partition_count > 1_000_000 {
        return invalid("probe transition validation exceeds one million partitions");
    }
    for east in east_edges.windows(2) {
        for north in north_edges.windows(2) {
            let sample_twice = [
                i128::from(east[0]) + i128::from(east[1]),
                i128::from(north[0]) + i128::from(north[1]),
            ];
            let active_spacing = policy
                .tiers
                .iter()
                .filter(|candidate| {
                    candidate
                        .regions_city_enu_mm
                        .iter()
                        .any(|region| contains_doubled_point(*region, sample_twice))
                })
                .map(|candidate| candidate.ground_spacing_m)
                .min();
            if let Some(active_spacing) = active_spacing
                && active_spacing > tier.ground_spacing_m
                && active_spacing != next_spacing
            {
                return invalid(format!(
                    "tier {:?} has a direct {} m to {} m transition without its {} m collar",
                    tier.id, tier.ground_spacing_m, active_spacing, next_spacing
                ));
            }
        }
    }
    Ok(())
}

fn contains_doubled_point(bounds: CityBoundsMm, point_twice: [i128; 2]) -> bool {
    (0..2).all(|axis| {
        i128::from(bounds.min[axis]) * 2 <= point_twice[axis]
            && point_twice[axis] <= i128::from(bounds.max[axis]) * 2
    })
}

#[derive(Clone)]
struct CandidateProbe {
    site: ProbeSite,
    precedence: (u32, String, String),
}

fn enumerate_probes(
    policy: &GradedProbePolicy,
    bounds_city_enu_mm: CityBoundsMm,
) -> Result<Vec<ProbeSite>> {
    let mut candidates = BTreeMap::<[i64; 3], CandidateProbe>::new();
    for tier in &policy.tiers {
        for region in &tier.regions_city_enu_mm {
            let Some(region) = region.intersection(bounds_city_enu_mm) else {
                continue;
            };
            enumerate_layer(
                policy,
                tier,
                &region,
                "ground",
                tier.ground_up_mm,
                tier.ground_spacing_m,
                &mut candidates,
            )?;
            for layer in &tier.elevated_layers {
                enumerate_layer(
                    policy,
                    tier,
                    &region,
                    &layer.id,
                    layer.up_mm,
                    layer.spacing_m,
                    &mut candidates,
                )?;
            }
        }
    }
    let mut probes = candidates
        .into_values()
        .map(|candidate| candidate.site)
        .collect::<Vec<_>>();
    probes.sort_by(|left, right| probe_sort_key(left).cmp(&probe_sort_key(right)));
    Ok(probes)
}

#[allow(clippy::too_many_arguments)]
fn enumerate_layer(
    policy: &GradedProbePolicy,
    tier: &ProbeTierPolicy,
    region: &CityBoundsMm,
    layer_id: &str,
    up_mm: i64,
    spacing_m: u32,
    candidates: &mut BTreeMap<[i64; 3], CandidateProbe>,
) -> Result<()> {
    let spacing_mm = i64::from(spacing_m)
        .checked_mul(MILLIMETRES_PER_METRE)
        .ok_or_else(|| invalid_error("probe spacing overflows millimetres"))?;
    let east_indices = lattice_indices(
        region.min[0],
        region.max[0],
        policy.lattice_origin_city_enu_mm[0],
        spacing_mm,
    )?;
    let north_indices = lattice_indices(
        region.min[1],
        region.max[1],
        policy.lattice_origin_city_enu_mm[1],
        spacing_mm,
    )?;
    let radius_mm = u32::try_from(spacing_mm)
        .map_err(|_| invalid_error("probe influence radius exceeds u32 millimetres"))?;
    for east_index in east_indices {
        let east_mm =
            lattice_position(policy.lattice_origin_city_enu_mm[0], east_index, spacing_mm)?;
        for north_index in north_indices.clone() {
            let north_mm = lattice_position(
                policy.lattice_origin_city_enu_mm[1],
                north_index,
                spacing_mm,
            )?;
            let position = [east_mm, north_mm, up_mm];
            let candidate = CandidateProbe {
                site: ProbeSite {
                    center_city_enu_mm: position,
                    radius_mm,
                    global_lattice_index: [east_index, north_index],
                    tier_id: tier.id.clone(),
                    layer_id: layer_id.to_owned(),
                },
                precedence: (spacing_m, tier.id.clone(), layer_id.to_owned()),
            };
            match candidates.get(&position) {
                Some(existing) if existing.precedence <= candidate.precedence => {}
                _ => {
                    candidates.insert(position, candidate);
                }
            }
        }
    }
    Ok(())
}

fn lattice_indices(min: i64, max: i64, origin: i64, spacing: i64) -> Result<Vec<i64>> {
    let relative_min = min
        .checked_sub(origin)
        .ok_or_else(|| invalid_error("lattice minimum relative coordinate overflows"))?;
    let relative_max = max
        .checked_sub(origin)
        .ok_or_else(|| invalid_error("lattice maximum relative coordinate overflows"))?;
    let first = div_ceil(relative_min, spacing);
    let last = relative_max.div_euclid(spacing);
    if first > last {
        return Ok(Vec::new());
    }
    let count = last
        .checked_sub(first)
        .and_then(|value| value.checked_add(1))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| invalid_error("lattice index count overflows"))?;
    if count > 1_000_000 {
        return invalid("one probe-layout axis exceeds one million sites");
    }
    Ok((first..=last).collect())
}

fn lattice_position(origin: i64, index: i64, spacing: i64) -> Result<i64> {
    index
        .checked_mul(spacing)
        .and_then(|offset| origin.checked_add(offset))
        .ok_or_else(|| invalid_error("global lattice position overflows"))
}

fn div_ceil(value: i64, divisor: i64) -> i64 {
    let quotient = value.div_euclid(divisor);
    quotient + i64::from(value.rem_euclid(divisor) != 0)
}

fn probe_sort_key(probe: &ProbeSite) -> (i64, i64, i64, u32, &str, &str) {
    (
        probe.center_city_enu_mm[2],
        probe.center_city_enu_mm[0],
        probe.center_city_enu_mm[1],
        probe.radius_mm,
        &probe.tier_id,
        &probe.layer_id,
    )
}

fn probe_layout_hash(probes: &[ProbeSite]) -> Result<String> {
    let count =
        u64::try_from(probes.len()).map_err(|_| invalid_error("probe layout count exceeds u64"))?;
    let mut bytes = Vec::with_capacity(32 + probes.len().saturating_mul(28));
    bytes.extend_from_slice(b"fightbox.probe-layout.v1\0");
    bytes.extend_from_slice(&count.to_le_bytes());
    for probe in probes {
        for coordinate in probe.center_city_enu_mm {
            bytes.extend_from_slice(&coordinate.to_le_bytes());
        }
        bytes.extend_from_slice(&probe.radius_mm.to_le_bytes());
    }
    Ok(sha256_hex(&bytes))
}

fn summarize_tiers(
    policy: &GradedProbePolicy,
    probes: &[ProbeSite],
) -> Result<Vec<ProbeTierSummary>> {
    let mut counts = BTreeMap::<(&str, &str), u64>::new();
    for probe in probes {
        let entry = counts.entry((&probe.tier_id, &probe.layer_id)).or_default();
        *entry = entry
            .checked_add(1)
            .ok_or_else(|| invalid_error("probe tier count overflows"))?;
    }
    policy
        .tiers
        .iter()
        .map(|tier| {
            let mut layers = Vec::with_capacity(tier.elevated_layers.len() + 1);
            layers.push(ProbeLayerSummary {
                id: "ground".to_owned(),
                up_mm: tier.ground_up_mm,
                spacing_m: tier.ground_spacing_m,
                probe_count: counts
                    .get(&(tier.id.as_str(), "ground"))
                    .copied()
                    .unwrap_or(0),
            });
            layers.extend(tier.elevated_layers.iter().map(|layer| {
                ProbeLayerSummary {
                    id: layer.id.clone(),
                    up_mm: layer.up_mm,
                    spacing_m: layer.spacing_m,
                    probe_count: counts
                        .get(&(tier.id.as_str(), layer.id.as_str()))
                        .copied()
                        .unwrap_or(0),
                }
            }));
            let probe_count = layers.iter().try_fold(0_u64, |total, layer| {
                total
                    .checked_add(layer.probe_count)
                    .ok_or_else(|| invalid_error("probe tier total overflows"))
            })?;
            Ok(ProbeTierSummary {
                id: tier.id.clone(),
                ground_spacing_m: tier.ground_spacing_m,
                analysis_spacing_m: tier.analysis_spacing_m,
                layers,
                probe_count,
            })
        })
        .collect()
}

fn estimate_bytes(probes: &[ProbeSite]) -> Result<ProbeByteEstimate> {
    let probe_count =
        u64::try_from(probes.len()).map_err(|_| invalid_error("probe count exceeds u64"))?;
    let reachable_ordered_pair_count = reachable_ordered_probe_pair_count(probes, 600)?;
    let estimated_raw_bytes = probe_count
        .checked_mul(PROBE_BYTES)
        .and_then(|bytes| {
            reachable_ordered_pair_count
                .checked_mul(REACHABLE_ORDERED_PAIR_BYTES)
                .and_then(|pairs| bytes.checked_add(pairs))
        })
        .and_then(|bytes| bytes.checked_add(SERIALIZATION_FIXED_BYTES))
        .ok_or_else(|| invalid_error("graded probe byte estimate overflows"))?;
    let projected_low_bytes = estimated_raw_bytes
        .checked_mul(7)
        .map(|bytes| bytes / 10)
        .ok_or_else(|| invalid_error("graded probe lower byte band overflows"))?;
    let projected_high_bytes = estimated_raw_bytes
        .checked_mul(13)
        .and_then(|bytes| bytes.checked_add(9))
        .map(|bytes| bytes / 10)
        .ok_or_else(|| invalid_error("graded probe upper byte band overflows"))?;
    Ok(ProbeByteEstimate {
        estimator_revision: PROBE_LAYOUT_ESTIMATOR_REVISION.to_owned(),
        path_horizon_m: 600,
        probe_count,
        reachable_ordered_pair_count,
        estimated_raw_bytes,
        projected_low_bytes,
        projected_high_bytes,
        target_raw_probe_payload_bytes: MOBILE_TARGET_RAW_PROBE_PAYLOAD_BYTES,
        hard_raw_probe_payload_bytes: MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES,
        estimate_within_target: estimated_raw_bytes <= MOBILE_TARGET_RAW_PROBE_PAYLOAD_BYTES,
        projected_high_within_hard_limit: projected_high_bytes
            <= MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES,
        calibrated_model: None,
    })
}

/// Counts directed, non-self probe pairs whose centres lie within the exact
/// caller-declared path horizon. The spatial binning is deterministic and uses
/// signed millimetres, so estimator and evidence tooling do not round through
/// floating point.
pub fn reachable_ordered_probe_pair_count(
    probes: &[ProbeSite],
    path_horizon_m: u32,
) -> Result<u64> {
    if path_horizon_m == 0 {
        return invalid("probe-pair path horizon must be positive");
    }
    let horizon_mm = i64::from(path_horizon_m)
        .checked_mul(MILLIMETRES_PER_METRE)
        .ok_or_else(|| invalid_error("probe-pair path horizon overflows millimetres"))?;
    let mut bins = BTreeMap::<[i64; 3], Vec<usize>>::new();
    let horizon_squared = i128::from(horizon_mm) * i128::from(horizon_mm);
    let mut ordered_pairs = 0_u64;
    for (index, probe) in probes.iter().enumerate() {
        let bin = probe
            .center_city_enu_mm
            .map(|coordinate| coordinate.div_euclid(horizon_mm));
        for east_offset in -1..=1 {
            for north_offset in -1..=1 {
                for up_offset in -1..=1 {
                    let candidate_bin = [
                        bin[0] + east_offset,
                        bin[1] + north_offset,
                        bin[2] + up_offset,
                    ];
                    let Some(candidate_indices) = bins.get(&candidate_bin) else {
                        continue;
                    };
                    for candidate_index in candidate_indices {
                        let other = &probes[*candidate_index];
                        let distance_squared = (0..3).try_fold(0_i128, |total, axis| {
                            let difference = i128::from(probe.center_city_enu_mm[axis])
                                - i128::from(other.center_city_enu_mm[axis]);
                            total.checked_add(difference * difference).ok_or_else(|| {
                                invalid_error("probe-pair squared distance overflows")
                            })
                        })?;
                        if distance_squared <= horizon_squared {
                            ordered_pairs = ordered_pairs.checked_add(2).ok_or_else(|| {
                                invalid_error("ordered probe-pair count overflows")
                            })?;
                        }
                    }
                }
            }
        }
        bins.entry(bin).or_default().push(index);
    }
    Ok(ordered_pairs)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AnalysisTileSeed {
    kind: AnalysisTileKind,
    tier_id: String,
    coarser_tier_id: Option<String>,
    bounds_city_enu_mm: CityBoundsMm,
    local_probe_spacing_m: u32,
    analysis_spacing_m: u32,
}

fn build_grid_set(
    policy: &GradedProbePolicy,
    cell: &CellProbeSlice,
    placement_layout_sha256: &str,
) -> Result<AnalysisGridSet> {
    let mut seeds = BTreeSet::new();
    for tier in &policy.tiers {
        for region in &tier.regions_city_enu_mm {
            if let Some(bounds) = region.intersection(cell.probe_footprint_bounds_city_enu_mm) {
                seeds.insert(AnalysisTileSeed {
                    kind: AnalysisTileKind::Tier,
                    tier_id: tier.id.clone(),
                    coarser_tier_id: None,
                    bounds_city_enu_mm: bounds,
                    local_probe_spacing_m: tier.ground_spacing_m,
                    analysis_spacing_m: tier.analysis_spacing_m,
                });
            }
        }
        let Some(next_spacing) = policy
            .tiers
            .iter()
            .map(|candidate| candidate.ground_spacing_m)
            .filter(|spacing| *spacing > tier.ground_spacing_m)
            .min()
        else {
            continue;
        };
        let width_mm = i64::from(next_spacing)
            .checked_mul(MILLIMETRES_PER_METRE)
            .ok_or_else(|| invalid_error("analysis collar width overflows"))?;
        for region in &tier.regions_city_enu_mm {
            let strips = boundary_collar_strips(*region, width_mm)?;
            for coarser in policy
                .tiers
                .iter()
                .filter(|candidate| candidate.ground_spacing_m == next_spacing)
            {
                for coarser_region in &coarser.regions_city_enu_mm {
                    for strip in strips {
                        let Some(bounds) = strip.intersection(*coarser_region).and_then(|bounds| {
                            bounds.intersection(cell.probe_footprint_bounds_city_enu_mm)
                        }) else {
                            continue;
                        };
                        seeds.insert(AnalysisTileSeed {
                            kind: AnalysisTileKind::BoundaryCollar,
                            tier_id: tier.id.clone(),
                            coarser_tier_id: Some(coarser.id.clone()),
                            bounds_city_enu_mm: bounds,
                            local_probe_spacing_m: tier.ground_spacing_m,
                            analysis_spacing_m: tier.analysis_spacing_m,
                        });
                    }
                }
            }
        }
    }
    let tiles = seeds
        .into_iter()
        .enumerate()
        .map(|(index, seed)| AnalysisTile {
            id: format!("tile-{index:04}"),
            kind: seed.kind,
            tier_id: seed.tier_id,
            coarser_tier_id: seed.coarser_tier_id,
            bounds_city_enu_mm: seed.bounds_city_enu_mm,
            local_probe_spacing_m: seed.local_probe_spacing_m,
            analysis_spacing_m: seed.analysis_spacing_m,
            placement_layout_sha256: placement_layout_sha256.to_owned(),
        })
        .collect();
    Ok(AnalysisGridSet {
        overlap_rule: GRID_OVERLAP_RULE.to_owned(),
        boundary_collar_rule: COLLAR_RULE.to_owned(),
        tiles,
    })
}

fn boundary_collar_strips(bounds: CityBoundsMm, width_mm: i64) -> Result<[CityBoundsMm; 4]> {
    let expanded = bounds.expand(width_mm, "analysis boundary collar")?;
    Ok([
        CityBoundsMm::new(
            [expanded.min[0], bounds.min[1]],
            [bounds.min[0], bounds.max[1]],
        ),
        CityBoundsMm::new(
            [bounds.max[0], bounds.min[1]],
            [expanded.max[0], bounds.max[1]],
        ),
        CityBoundsMm::new(
            [expanded.min[0], expanded.min[1]],
            [expanded.max[0], bounds.min[1]],
        ),
        CityBoundsMm::new(
            [expanded.min[0], bounds.max[1]],
            [expanded.max[0], expanded.max[1]],
        ),
    ])
}

fn verify_world_constants() -> Result<()> {
    if CELL_PROBE_FOOTPRINT_M.to_bits() != 585.0_f64.to_bits()
        || CELL_STRIDE_M.to_bits() != 485.0_f64.to_bits()
        || CELL_PAIRWISE_OVERLAP_M.to_bits() != 100.0_f64.to_bits()
        || CELL_OWNERSHIP_GUARD_M.to_bits() != 50.0_f64.to_bits()
        || CELL_GEOMETRY_HALO_M.to_bits() != 600.0_f64.to_bits()
        || MOBILE_BAKED_PATH_HORIZON_M.to_bits() != 600.0_f64.to_bits()
        || CELL_PROBE_FOOTPRINT_MM - CELL_STRIDE_MM != CELL_PAIRWISE_OVERLAP_MM
        || CELL_PAIRWISE_OVERLAP_MM / 2 != CELL_OWNERSHIP_GUARD_MM
    {
        return invalid("graded probe planner constants differ from world-package v2");
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
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fightbox-probe-plan-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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

    fn production_fixture_policy() -> GradedProbePolicy {
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

    #[test]
    fn cell_geometry_overlap_and_half_open_ownership_are_exact() {
        let west = CellProbeSlice::for_grid_index(CellGridIndex { east: 0, north: 0 }).unwrap();
        let east = CellProbeSlice::for_grid_index(CellGridIndex { east: 1, north: 0 }).unwrap();
        let overlap = west
            .probe_footprint_bounds_city_enu_mm
            .intersection(east.probe_footprint_bounds_city_enu_mm)
            .unwrap();
        assert_eq!(overlap.max[0] - overlap.min[0], 100_000);
        assert_eq!(
            west.ownership_bounds_city_enu_mm.max[0],
            east.ownership_bounds_city_enu_mm.min[0]
        );
        let switch_east = west.ownership_bounds_city_enu_mm.max[0];
        assert_eq!(
            west.classify_listener_city_enu_mm([switch_east - 1, 0]),
            ListenerOwnership::Owned
        );
        assert_eq!(
            east.classify_listener_city_enu_mm([switch_east - 1, 0]),
            ListenerOwnership::Guard
        );
        assert_eq!(
            west.classify_listener_city_enu_mm([switch_east, 0]),
            ListenerOwnership::Guard
        );
        assert_eq!(
            east.classify_listener_city_enu_mm([switch_east, 0]),
            ListenerOwnership::Owned
        );
        assert_eq!(
            west.geometry_material_bounds_city_enu_mm.min[0],
            west.probe_footprint_bounds_city_enu_mm.min[0] - 600_000
        );
        assert_eq!(
            west.geometry_material_bounds_city_enu_mm.max[1],
            west.probe_footprint_bounds_city_enu_mm.max[1] + 600_000
        );
    }

    #[test]
    fn neighboring_cells_have_identical_probe_records_in_the_shared_footprint() {
        let policy = production_fixture_policy();
        let west = policy
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        let east = policy
            .plan_cell(CellGridIndex { east: 1, north: 0 })
            .unwrap();
        let overlap = west
            .cell
            .probe_footprint_bounds_city_enu_mm
            .intersection(east.cell.probe_footprint_bounds_city_enu_mm)
            .unwrap();
        let shared_west = west
            .probes
            .iter()
            .filter(|probe| {
                overlap.contains_closed([probe.center_city_enu_mm[0], probe.center_city_enu_mm[1]])
            })
            .cloned()
            .collect::<Vec<_>>();
        let shared_east = east
            .probes
            .iter()
            .filter(|probe| {
                overlap.contains_closed([probe.center_city_enu_mm[0], probe.center_city_enu_mm[1]])
            })
            .cloned()
            .collect::<Vec<_>>();
        assert!(!shared_west.is_empty());
        assert_eq!(shared_west, shared_east);
        assert!(
            shared_west
                .iter()
                .any(|probe| probe.tier_id == "route-core")
        );
        assert!(
            shared_west
                .iter()
                .any(|probe| probe.tier_id == "transition")
        );
        assert!(shared_west.iter().any(|probe| probe.tier_id == "residual"));
        assert!(
            shared_west
                .iter()
                .any(|probe| probe.center_city_enu_mm[2] == 63_000)
        );
    }

    #[test]
    fn shuffled_policy_declarations_resolve_to_identical_bytes_and_hashes() {
        let original = production_fixture_policy();
        let mut shuffled = original.clone();
        shuffled.tiers.reverse();
        for tier in &mut shuffled.tiers {
            tier.regions_city_enu_mm.reverse();
            tier.elevated_layers.reverse();
        }
        let original = original
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        let shuffled = shuffled
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        assert_eq!(original, shuffled);
        assert_eq!(
            original.to_sidecar_bytes().unwrap(),
            shuffled.to_sidecar_bytes().unwrap()
        );
    }

    #[test]
    fn arbitrary_bounds_reuse_the_exact_cell_lattice_without_cell_semantics() {
        let policy = production_fixture_policy();
        let cell = CellProbeSlice::for_grid_index(CellGridIndex { east: 0, north: 0 }).unwrap();
        let resolved = policy
            .resolve_bounds(cell.probe_footprint_bounds_city_enu_mm)
            .unwrap();
        let planned = policy
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        assert_eq!(
            resolved.bounds_city_enu_mm,
            cell.probe_footprint_bounds_city_enu_mm
        );
        assert_eq!(
            resolved.placement_policy_sha256,
            planned.placement_policy_sha256
        );
        assert_eq!(resolved.probe_layout_sha256, planned.probe_layout_sha256);
        assert_eq!(resolved.tier_summaries, planned.tier_summaries);
        assert_eq!(resolved.probes, planned.probes);

        let oracle = policy
            .resolve_bounds(CityBoundsMm::new([-342_500, -342_500], [827_500, 827_500]))
            .unwrap();
        assert!(oracle.probes.len() > resolved.probes.len());
        let unique = oracle
            .probes
            .iter()
            .map(|probe| probe.center_city_enu_mm)
            .collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), oracle.probes.len());
    }

    #[test]
    fn reachable_pair_counter_honors_the_caller_horizon() {
        let probe = |east_mm| ProbeSite {
            center_city_enu_mm: [east_mm, 0, 1_500],
            radius_mm: 4_000,
            global_lattice_index: [east_mm / 1_000, 0],
            tier_id: "test".to_owned(),
            layer_id: "ground".to_owned(),
        };
        let probes = vec![probe(0), probe(600_000), probe(1_750_000)];
        assert_eq!(reachable_ordered_probe_pair_count(&probes, 599).unwrap(), 0);
        assert_eq!(reachable_ordered_probe_pair_count(&probes, 600).unwrap(), 2);
        assert_eq!(
            reachable_ordered_probe_pair_count(&probes, 1_750).unwrap(),
            6
        );
        assert!(reachable_ordered_probe_pair_count(&probes, 0).is_err());
    }

    #[test]
    fn grid_set_prefers_the_fine_constant_analysis_boundary_collar() {
        let plan = production_fixture_policy()
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        assert!(plan.grid_set.tiles.iter().all(|tile| {
            tile.placement_layout_sha256 == plan.probe_layout_sha256
                && matches!(tile.analysis_spacing_m, 4 | 8 | 16 | 32)
        }));
        let tile = plan
            .grid_set
            .tile_for_city_enu_mm([100_000, 30_000])
            .unwrap();
        assert_eq!(tile.kind, AnalysisTileKind::BoundaryCollar);
        assert_eq!(tile.tier_id, "route-core");
        assert_eq!(tile.coarser_tier_id.as_deref(), Some("transition"));
        assert_eq!(tile.local_probe_spacing_m, 8);
        assert_eq!(tile.analysis_spacing_m, 8);
    }

    #[test]
    fn sidecar_round_trips_strictly_and_indexes_as_a_required_v2_capability() {
        let plan = production_fixture_policy()
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        assert_eq!(plan.policy.sky_pathing_policy.maximum_layer_m, 63);
        assert_eq!(
            plan.policy.sky_pathing_policy.above,
            AboveMaximumLayerPolicy::DirectReflectionsOnly
        );
        assert_eq!(
            plan.byte_estimate.estimated_raw_bytes,
            plan.byte_estimate.probe_count * 256
                + plan.byte_estimate.reachable_ordered_pair_count * 10
                + 64 * 1_024
        );
        assert_eq!(
            plan.placement_policy_sha256,
            "a4962b8ff925ac64f9bc09f82ce6fe58c2e344c4c471e6ea74bebb28b2623f08"
        );
        assert_eq!(
            plan.probe_layout_sha256,
            "c0d77386e9f349127bf3315f26e454595a359fa26a117d3d0111094f09873412"
        );
        assert_eq!(plan.byte_estimate.probe_count, 1_826);
        assert_eq!(plan.byte_estimate.reachable_ordered_pair_count, 3_330_164);
        assert_eq!(plan.byte_estimate.estimated_raw_bytes, 33_834_632);
        assert_eq!(plan.byte_estimate.projected_low_bytes, 23_684_242);
        assert_eq!(plan.byte_estimate.projected_high_bytes, 43_985_022);
        assert!(plan.byte_estimate.estimate_within_target);
        assert!(plan.byte_estimate.projected_high_within_hard_limit);

        let bytes = plan.to_sidecar_bytes().unwrap();
        assert_eq!(
            CityBakeV2ProbePlan::from_sidecar_bytes(&bytes).unwrap(),
            plan
        );
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["unknown"] = serde_json::Value::Bool(true);
        assert!(
            CityBakeV2ProbePlan::from_sidecar_bytes(&serde_json::to_vec(&value).unwrap()).is_err()
        );

        let directory = TestDirectory::new();
        let extension = write_city_bake_v2_plan_sidecar(&directory.0, &plan).unwrap();
        assert_eq!(extension.capability, CITY_BAKE_V2_CAPABILITY);
        assert_eq!(extension.requirement, ExtensionRequirement::Required);
        assert_eq!(extension.path, CITY_BAKE_V2_SIDECAR_PATH);
        assert_eq!(
            fs::read(directory.0.join(CITY_BAKE_V2_SIDECAR_PATH)).unwrap(),
            bytes
        );
    }

    #[test]
    fn completed_sidecar_cross_binds_plan_cell_batch_and_sky_policy() {
        let plan = production_fixture_policy()
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        let count = plan.byte_estimate.probe_count;
        let completed = CityBakeV2BakedArtifact::bind_successful_bake(
            &plan,
            "chicago",
            "chicago:e0:n0",
            "1".repeat(64),
            "2".repeat(64),
            CityBakeV2PathBakeSettings {
                num_visibility_samples: 1,
                probe_visibility_radius_m: 0.0,
                visibility_threshold: 0.5,
                visibility_range_m: 6.0,
                path_range_m: 600.0,
                num_threads: 1,
            },
            CityBakeV2ProbeBatchIdentity {
                capability: crate::STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY.to_owned(),
                payload_path: "probe-batch.bin".to_owned(),
                compression: crate::PackageCompression::None,
                metadata_schema: "fightbox.steam-audio.probe-batch.v1".to_owned(),
                steam_audio_version: "4.8.1".to_owned(),
                upstream_commit: "0da1825".to_owned(),
                probe_count: count,
                path_data_size_bytes: 15_000_000,
                serialized_size_bytes: 20_000_000,
                serialized_sha256: "3".repeat(64),
            },
            CityBakeV2DeterministicTelemetry {
                baker_revision: "steam-audio-explicit-probes-v1".to_owned(),
                submitted_explicit_probe_count: count,
                committed_probe_count: count,
                insertion_order: "probe_plan_local_centres".to_owned(),
                bake_progress_callback_count: 11,
                final_bake_progress_millionths: 1_000_000,
            },
        )
        .unwrap();
        assert_eq!(
            completed.sky_pathing_policy.above,
            AboveMaximumLayerPolicy::DirectReflectionsOnly
        );
        assert_eq!(completed.probe_batch.probe_count, 1_826);
        let bytes = completed.to_sidecar_bytes().unwrap();
        assert_eq!(
            CityBakeV2BakedArtifact::from_sidecar_bytes(&bytes).unwrap(),
            completed
        );

        let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        corrupt["probe_batch"]["probe_count"] = serde_json::json!(1_825);
        assert!(
            CityBakeV2BakedArtifact::from_sidecar_bytes(&serde_json::to_vec(&corrupt).unwrap())
                .is_err()
        );
    }

    #[test]
    fn policy_rejects_non_production_spacing_and_missing_maximum_layer() {
        let mut policy = production_fixture_policy();
        policy.tiers[0].ground_spacing_m = 12;
        assert!(matches!(
            policy.plan_cell(CellGridIndex { east: 0, north: 0 }),
            Err(WorldError::InvalidPackage(message)) if message.contains("production tiers")
        ));

        let mut policy = production_fixture_policy();
        for tier in &mut policy.tiers {
            tier.elevated_layers.retain(|layer| layer.up_mm != 63_000);
        }
        assert!(matches!(
            policy.plan_cell(CellGridIndex { east: 0, north: 0 }),
            Err(WorldError::InvalidPackage(message)) if message.contains("63 m")
        ));
    }

    #[test]
    fn mesh_observable_deduplicates_edges_and_blocks_inclusive_contacts() {
        let probes = vec![
            ProbeSite {
                center_city_enu_mm: [0, 1_000, 1_500],
                radius_mm: 4_000,
                global_lattice_index: [0, 0],
                tier_id: "owner-home".to_owned(),
                layer_id: "ground".to_owned(),
            },
            ProbeSite {
                center_city_enu_mm: [2_000, 1_000, 1_500],
                radius_mm: 4_000,
                global_lattice_index: [1, 0],
                tier_id: "owner-home".to_owned(),
                layer_id: "ground".to_owned(),
            },
        ];
        let mesh = AcousticMesh {
            vertices_enu_m: vec![
                fightbox_api::EnuVector3::new(1.0, 0.0, 1.0),
                fightbox_api::EnuVector3::new(1.0, 2.0, 1.0),
                fightbox_api::EnuVector3::new(1.0, 0.0, 2.0),
            ],
            triangles: vec![[0, 1, 2], [2, 1, 0]],
            material_ids: vec![0, 0],
        };
        let observable = mesh_open_pair_observable(&mesh, &probes, [0, 0, 0]).unwrap();
        assert_eq!(observable.wall_edge_count, 1);
        assert_eq!(observable.blocked_owner_ground_ordered_pair_count, 2);
        assert_eq!(observable.open_owner_ground_ordered_pair_count, 0);
    }

    #[test]
    fn calibrated_model_rejects_tier_and_arithmetic_tampering() {
        let policy = production_fixture_policy();
        let plan = policy
            .plan_cell(CellGridIndex { east: 0, north: 0 })
            .unwrap();
        let mesh = AcousticMesh {
            vertices_enu_m: vec![],
            triangles: vec![],
            material_ids: vec![],
        };
        assert!(calibrated_probe_byte_model(&plan, &mesh).is_err());
        let observable = MeshOpenPairObservable {
            algorithm: FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM.to_owned(),
            coordinate_encoding: FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING.to_owned(),
            mesh_sha256: None,
            owner_ground_height_mm: 1_500,
            owner_ground_probe_count: 2,
            wall_edge_count: 0,
            blocked_owner_ground_ordered_pair_count: 0,
            open_owner_ground_ordered_pair_count: 2,
        };
        let mut model = CalibratedProbeByteModelV2 {
            revision: FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION.to_owned(),
            fixed_bytes: 6_000,
            owner_home_bytes_per_probe: 3_315,
            route_core_bytes_per_probe: 31,
            transition_bytes_per_probe: 54,
            residual_bytes_per_probe: 30,
            open_owner_ground_ordered_pair_bytes: 14,
            owner_home_probe_count: 2,
            route_core_probe_count: 1,
            transition_probe_count: 1,
            residual_probe_count: 1,
            owner_ground_probe_count: 2,
            open_owner_ground_ordered_pair_count: 2,
            point_estimate_bytes: 0,
            projected_low_bytes: 0,
            projected_high_bytes: 0,
            reservation_bytes: 0,
            mesh_observable: observable,
        };
        assert!(model.validate().is_err());
        model.point_estimate_bytes = u64::MAX;
        assert!(model.validate().is_err());
    }

    #[test]
    fn mesh_open_pairs_counts_ordered_crossings_and_open_pairs() {
        let mesh = AcousticMesh {
            vertices_enu_m: vec![
                fightbox_api::EnuVector3::new(5.0, -5.0, 1.0),
                fightbox_api::EnuVector3::new(5.0, 5.0, 2.0),
                fightbox_api::EnuVector3::new(5.0, 0.0, 3.0),
            ],
            triangles: vec![[0, 1, 2]],
            material_ids: vec![0],
        };
        let probe = |east_mm, north_mm| ProbeSite {
            center_city_enu_mm: [east_mm, north_mm, 1_500],
            radius_mm: 4_000,
            global_lattice_index: [east_mm / 1_000, north_mm / 1_000],
            tier_id: "owner-home".to_owned(),
            layer_id: "ground".to_owned(),
        };
        let observable = mesh_open_pair_observable(
            &mesh,
            &[probe(0, 0), probe(10_000, 0), probe(0, 10_000)],
            [0, 0, 0],
        )
        .unwrap();
        assert_eq!(observable.owner_ground_height_mm, 1_500);
        assert_eq!(observable.owner_ground_probe_count, 3);
        assert_eq!(observable.blocked_owner_ground_ordered_pair_count, 4);
        assert_eq!(observable.open_owner_ground_ordered_pair_count, 2);
    }

    #[test]
    fn mesh_open_pairs_accepts_zero_owner_and_exact_collinear_contact() {
        let mesh = AcousticMesh {
            vertices_enu_m: vec![
                fightbox_api::EnuVector3::new(5.0, -5.0, 1.0),
                fightbox_api::EnuVector3::new(5.0, 5.0, 2.0),
                fightbox_api::EnuVector3::new(5.0, 0.0, 3.0),
            ],
            triangles: vec![[0, 1, 2]],
            material_ids: vec![0],
        };
        let observable = mesh_open_pair_observable(&mesh, &[], [0, 0, 0]).unwrap();
        assert_eq!(observable.owner_ground_height_mm, 1_500);
        assert_eq!(observable.owner_ground_probe_count, 0);
        assert_eq!(observable.blocked_owner_ground_ordered_pair_count, 0);
        assert_eq!(observable.open_owner_ground_ordered_pair_count, 0);
        assert!(closed_segments_intersect([0, 0], [10, 0], [5, 0], [5, 10]).unwrap());
        assert!(closed_segments_intersect([0, 0], [10, 0], [5, 0], [8, 0]).unwrap());
        assert!(!closed_segments_intersect([0, 0], [10, 0], [5, 1], [8, 1]).unwrap());
    }

    #[test]
    fn segment_orientation_fails_closed_on_i128_overflow() {
        assert!(
            orientation(
                [i64::MIN, i64::MIN],
                [i64::MAX, i64::MAX],
                [i64::MIN, i64::MAX]
            )
            .is_err()
        );
    }

    #[test]
    fn policy_rejects_a_spatially_direct_8m_to_32m_boundary() {
        let mut policy = production_fixture_policy();
        let transition = policy
            .tiers
            .iter_mut()
            .find(|tier| tier.id == "transition")
            .unwrap();
        transition.regions_city_enu_mm = vec![region(1_000, 1_000, 1_100, 1_100)];
        assert!(matches!(
            policy.plan_cell(CellGridIndex { east: 0, north: 0 }),
            Err(WorldError::InvalidPackage(message))
                if message.contains("direct 8 m to 32 m transition")
        ));
    }
}
