//! Strict, standalone Wave 17 probe-byte estimator envelope.
//!
//! This module deliberately does not participate in any bake or oracle output
//! path yet.  It gives those later consumers one typed, hash-bound estimate
//! contract without changing an existing v1/v2 artifact.

use fightbox_evidence::sha256_hex;
use fightbox_world::{
    CALIBRATED_MOBILE_PLACEMENT_POLICY_SHA256, FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM,
    FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING, FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION,
};
use serde::{Deserialize, Serialize};

/// Schema identifier for the additive estimator envelope.
pub(crate) const PROBE_BYTE_ESTIMATE_V2_SCHEMA: &str = "fightbox.probe-byte-estimate.v2";
pub(crate) const PROBE_BYTE_ESTIMATE_V2_FILENAME: &str = "probe-byte-estimate-v2.json";
pub(crate) const PROVISIONAL_ESTIMATOR_REVISION: &str =
    "reachable-ordered-pairs-p256-q10-fixed64k-v1";
const PROBE_BYTES_V1: u64 = 256;
const PAIR_BYTES_V1: u64 = 10;
const FIXED_BYTES_V1: u64 = 64 * 1_024;
const FIXED_BYTES_V2: u64 = 6_000;
const OWNER_HOME_BYTES_V2: u64 = 3_315;
const ROUTE_CORE_BYTES_V2: u64 = 31;
const TRANSITION_BYTES_V2: u64 = 54;
const RESIDUAL_BYTES_V2: u64 = 30;
const OPEN_OWNER_GROUND_PAIR_BYTES_V2: u64 = 14;
const MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES: u64 = 64 * 1_024 * 1_024;
/// Domain separator for the request identity hash.
///
/// The terminating NUL is part of the domain and prevents this identity from
/// being confused with a hash of an adjacent textual artifact.
pub(crate) const REQUEST_SHA256_DOMAIN: &[u8] = b"fightbox.probe-byte-estimate.request.v2\0";

/// The manifest/package identity to which an estimate applies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateSubjectV2 {
    pub schema_version: String,
    pub manifest_path: String,
    pub manifest_sha256: String,
    pub package_manifest_sha256: String,
}

/// Pathing controls that affect the estimate's request identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimatePathingV2 {
    pub visibility_range_m: f32,
    pub visibility_samples: i32,
    pub visibility_threshold: f32,
    pub probe_visibility_radius_m: f32,
    pub threads: i32,
}

/// Pinned SDK/baker identity that affects serialized probe data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateSdkV2 {
    pub metadata_schema: String,
    pub steam_audio_version: String,
    pub upstream_commit: String,
    pub baker_revision: String,
}

/// The canonical request whose identity is bound by [`request_sha256`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateRequestV2 {
    pub request_sha256: String,
    pub mesh_sha256: String,
    pub materials_sha256: String,
    pub probe_plan_sha256: Option<String>,
    pub placement_policy_sha256: Option<String>,
    pub probe_layout_sha256: Option<String>,
    pub probe_count: u64,
    pub path_horizon_m: u32,
    pub pathing: ProbeByteEstimatePathingV2,
    pub sdk: ProbeByteEstimateSdkV2,
}

/// Whether `pair_count` is a spatially exact count or an unculled bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProbeByteEstimatePairCountKindV2 {
    Exact,
    UnculledUpperBound,
}

/// Arithmetic model used to form the estimate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateTierCountsV2 {
    pub owner_home_count: u64,
    pub route_core_count: u64,
    pub transition_count: u64,
    pub residual_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateMeshObservableV2 {
    pub algorithm: String,
    pub coordinate_encoding: String,
    pub mesh_sha256: String,
    pub owner_ground_height_mm: i64,
    pub owner_ground_probe_count: u64,
    pub wall_edge_count: u64,
    pub blocked_owner_ground_ordered_pair_count: u64,
    pub open_owner_ground_ordered_pair_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateCalibratedModelV2 {
    pub revision: String,
    pub fixed_bytes: u64,
    pub owner_home_bytes_per_probe: u64,
    pub route_core_bytes_per_probe: u64,
    pub transition_bytes_per_probe: u64,
    pub residual_bytes_per_probe: u64,
    pub open_owner_ground_ordered_pair_bytes: u64,
    pub tier_counts: ProbeByteEstimateTierCountsV2,
    pub point_bytes: u64,
    pub projected_low_bytes: u64,
    pub projected_high_bytes: u64,
    pub reservation_bytes: u64,
    pub mesh_observable: ProbeByteEstimateMeshObservableV2,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateModelV2 {
    pub revision: String,
    pub probe_bytes: u64,
    pub pair_bytes: u64,
    pub fixed_bytes: u64,
    pub pair_count: u64,
    pub pair_count_kind: ProbeByteEstimatePairCountKindV2,
    pub estimated_raw_bytes: u64,
    pub projected_low_bytes: u64,
    pub projected_high_bytes: u64,
    pub reservation_bytes: u64,
    /// Additive fields are omitted for the frozen Q×10 revision, preserving
    /// its canonical JSON bytes exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibrated_model: Option<ProbeByteEstimateCalibratedModelV2>,
}

/// Measurements from the completed artifact, kept separate from the model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateObservedV2 {
    pub probe_count: u64,
    pub path_data_size_bytes: u64,
    pub serialized_size_bytes: u64,
    pub payload_sha256: String,
    pub artifact_bytes: u64,
}

/// Strict root envelope for a completed probe-byte estimate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeByteEstimateV2 {
    pub schema_version: String,
    pub artifact_state: String,
    pub subject: ProbeByteEstimateSubjectV2,
    pub request: ProbeByteEstimateRequestV2,
    pub model: ProbeByteEstimateModelV2,
    pub observed: ProbeByteEstimateObservedV2,
}

impl ProbeByteEstimateV2 {
    /// Validate the envelope, including request hash and checked arithmetic.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != PROBE_BYTE_ESTIMATE_V2_SCHEMA {
            return Err(format!(
                "schema_version must be {PROBE_BYTE_ESTIMATE_V2_SCHEMA}"
            ));
        }
        if self.artifact_state != "completed" {
            return Err("artifact_state must be completed".to_owned());
        }

        let expected_manifest_path = match self.subject.schema_version.as_str() {
            "fightbox.city-bake.v1" => "city-bake-manifest.json",
            "fightbox.city-bake.v2" => "capabilities/city-bake-v2.json",
            "fightbox.city-oracle.v1" => "city-oracle-manifest.json",
            _ => return Err("subject.schema_version is not a supported bake authority".to_owned()),
        };
        if self.subject.manifest_path != expected_manifest_path {
            return Err(format!(
                "subject.manifest_path must be {expected_manifest_path} for {}",
                self.subject.schema_version
            ));
        }
        hash(&self.subject.manifest_sha256, "subject.manifest_sha256")?;
        hash(
            &self.subject.package_manifest_sha256,
            "subject.package_manifest_sha256",
        )?;

        hash(&self.request.mesh_sha256, "request.mesh_sha256")?;
        hash(&self.request.materials_sha256, "request.materials_sha256")?;
        optional_hash(
            self.request.probe_plan_sha256.as_deref(),
            "request.probe_plan_sha256",
        )?;
        optional_hash(
            self.request.placement_policy_sha256.as_deref(),
            "request.placement_policy_sha256",
        )?;
        optional_hash(
            self.request.probe_layout_sha256.as_deref(),
            "request.probe_layout_sha256",
        )?;
        if self.request.probe_count == 0 {
            return Err("request.probe_count must be greater than zero".to_owned());
        }
        if self.request.path_horizon_m == 0 {
            return Err("request.path_horizon_m must be greater than zero".to_owned());
        }
        validate_pathing(&self.request.pathing)?;
        validate_sdk(&self.request.sdk)?;

        let expected_request_sha256 = request_sha256(&self.request)?;
        if self.request.request_sha256 != expected_request_sha256 {
            return Err(format!(
                "request.request_sha256 does not bind the canonical request (expected {expected_request_sha256})"
            ));
        }

        if self.model.revision == FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION {
            self.validate_fixed_tier_mesh_model()?;
        } else if self.model.revision == PROVISIONAL_ESTIMATOR_REVISION {
            if self.model.calibrated_model.is_some() {
                return Err("legacy v1 estimator must not carry calibrated model fields".to_owned());
            }
            let expected_raw = self
                .request
                .probe_count
                .checked_mul(self.model.probe_bytes)
                .and_then(|bytes| {
                    self.model
                        .pair_count
                        .checked_mul(self.model.pair_bytes)
                        .and_then(|pairs| bytes.checked_add(pairs))
                })
                .and_then(|bytes| bytes.checked_add(self.model.fixed_bytes))
                .ok_or_else(|| "model byte arithmetic overflows u64".to_owned())?;
            if self.model.probe_bytes != PROBE_BYTES_V1
                || self.model.pair_bytes != PAIR_BYTES_V1
                || self.model.fixed_bytes != FIXED_BYTES_V1
            {
                return Err(
                    "model revision and coefficients differ from the provisional v1 estimator"
                        .to_owned(),
                );
            }
            if self.model.estimated_raw_bytes != expected_raw {
                return Err(format!(
                    "model.estimated_raw_bytes must equal probe_count*probe_bytes + pair_count*pair_bytes + fixed_bytes ({expected_raw})"
                ));
            }
            let maximum_ordered_pairs = self
                .request
                .probe_count
                .checked_mul(self.request.probe_count - 1)
                .ok_or_else(|| "ordered pair count overflows u64".to_owned())?;
            if self.model.pair_count > maximum_ordered_pairs {
                return Err(format!(
                    "model.pair_count exceeds the maximum ordered pair count ({maximum_ordered_pairs})"
                ));
            }
            if self.model.pair_count_kind == ProbeByteEstimatePairCountKindV2::UnculledUpperBound
                && self.model.pair_count != maximum_ordered_pairs
            {
                return Err("unculled_upper_bound pair count must equal P*(P-1)".to_owned());
            }
            let expected_low = self
                .model
                .estimated_raw_bytes
                .checked_mul(7)
                .map(|bytes| bytes / 10)
                .ok_or_else(|| "model lower projection overflows u64".to_owned())?;
            let expected_high = self
                .model
                .estimated_raw_bytes
                .checked_mul(13)
                .and_then(|bytes| bytes.checked_add(9))
                .map(|bytes| bytes / 10)
                .ok_or_else(|| "model upper projection overflows u64".to_owned())?;
            if self.model.projected_low_bytes != expected_low
                || self.model.projected_high_bytes != expected_high
            {
                return Err(
                    "model projected byte band must be the exact floor/ceiling ±30% band"
                        .to_owned(),
                );
            }
            if self.model.reservation_bytes != self.model.projected_high_bytes {
                return Err(
                    "model.reservation_bytes must equal the exact projected_high_bytes reservation"
                        .to_owned(),
                );
            }
        } else {
            return Err("unknown probe-byte estimator model revision".to_owned());
        }

        if self.observed.probe_count != self.request.probe_count {
            return Err(
                "observed.probe_count must equal request.probe_count for a completed artifact"
                    .to_owned(),
            );
        }
        if self.observed.path_data_size_bytes > self.observed.serialized_size_bytes {
            return Err(
                "observed.path_data_size_bytes must not exceed serialized_size_bytes".to_owned(),
            );
        }
        if self.observed.serialized_size_bytes == 0 {
            return Err("observed.serialized_size_bytes must be greater than zero".to_owned());
        }
        if self.observed.serialized_size_bytes > self.model.projected_high_bytes {
            return Err(
                "observed.serialized_size_bytes must not exceed projected_high_bytes".to_owned(),
            );
        }
        if self.observed.artifact_bytes < self.observed.serialized_size_bytes {
            return Err("observed.artifact_bytes must cover serialized_size_bytes".to_owned());
        }
        if self.model.revision == FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION {
            if self.observed.artifact_bytes < self.model.projected_low_bytes
                || self.observed.artifact_bytes > self.model.projected_high_bytes
            {
                return Err(
                    "strict calibrated observed.artifact_bytes must lie within projected_low_bytes..projected_high_bytes"
                        .to_owned(),
                );
            }
        } else if self.observed.artifact_bytes > self.model.reservation_bytes {
            return Err("observed.artifact_bytes must fit within reservation_bytes".to_owned());
        }
        hash(&self.observed.payload_sha256, "observed.payload_sha256")?;
        Ok(())
    }

    /// Serialize the validated envelope as deterministic compact JSON.
    pub(crate) fn to_canonical_json(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|error| format!("cannot serialize estimate JSON: {error}"))
    }

    /// Parse and validate an estimate envelope from JSON.
    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let estimate: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("cannot parse estimate JSON: {error}"))?;
        estimate.validate()?;
        Ok(estimate)
    }

    /// Resolve the self-size contribution to `observed.artifact_bytes` and
    /// return the final canonical envelope. The subject hash never includes
    /// this additive file, so no authority cycle is introduced.
    pub(crate) fn finalize_canonical_json(
        mut self,
        artifact_bytes_without_envelope: u64,
    ) -> Result<(Self, Vec<u8>), String> {
        self.observed.artifact_bytes = artifact_bytes_without_envelope;
        for _ in 0..8 {
            let bytes = self.to_canonical_json()?;
            let total = artifact_bytes_without_envelope
                .checked_add(
                    u64::try_from(bytes.len())
                        .map_err(|_| "estimate envelope length exceeds u64".to_owned())?,
                )
                .ok_or_else(|| "estimate artifact byte count overflows u64".to_owned())?;
            if self.observed.artifact_bytes == total {
                return Ok((self, bytes));
            }
            self.observed.artifact_bytes = total;
        }
        Err("estimate envelope byte-size fixed point did not converge".to_owned())
    }

    fn validate_fixed_tier_mesh_model(&self) -> Result<(), String> {
        if self.subject.schema_version != "fightbox.city-bake.v2"
            || self.request.path_horizon_m != 600
            || self.request.probe_plan_sha256.is_none()
            || self.request.probe_layout_sha256.is_none()
            || self.request.placement_policy_sha256.as_deref()
                != Some(CALIBRATED_MOBILE_PLACEMENT_POLICY_SHA256)
        {
            return Err(
                "fixed tier/mesh estimator is admitted only for the calibrated mobile policy"
                    .to_owned(),
            );
        }
        if self.model.probe_bytes != 0
            || self.model.pair_bytes != 0
            || self.model.fixed_bytes != 0
            || self.model.pair_count != 0
            || self.model.pair_count_kind != ProbeByteEstimatePairCountKindV2::Exact
        {
            return Err("fixed tier/mesh model must not carry legacy coefficients".to_owned());
        }
        let calibrated = self
            .model
            .calibrated_model
            .as_ref()
            .ok_or_else(|| "fixed tier/mesh model is missing calibrated_model".to_owned())?;
        if calibrated.revision != FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION
            || calibrated.fixed_bytes != FIXED_BYTES_V2
            || calibrated.owner_home_bytes_per_probe != OWNER_HOME_BYTES_V2
            || calibrated.route_core_bytes_per_probe != ROUTE_CORE_BYTES_V2
            || calibrated.transition_bytes_per_probe != TRANSITION_BYTES_V2
            || calibrated.residual_bytes_per_probe != RESIDUAL_BYTES_V2
            || calibrated.open_owner_ground_ordered_pair_bytes != OPEN_OWNER_GROUND_PAIR_BYTES_V2
        {
            return Err(
                "fixed tier/mesh calibrated coefficients differ from the frozen Wave 17 model"
                    .to_owned(),
            );
        }
        let point = calibrated.point_bytes;
        let tiers = &calibrated.tier_counts;
        let mesh = &calibrated.mesh_observable;
        let tier_total = tiers
            .owner_home_count
            .checked_add(tiers.route_core_count)
            .and_then(|v| v.checked_add(tiers.transition_count))
            .and_then(|v| v.checked_add(tiers.residual_count))
            .ok_or_else(|| "fixed tier count arithmetic overflows u64".to_owned())?;
        if tier_total != self.request.probe_count {
            return Err("fixed tier counts must sum to request.probe_count".to_owned());
        }
        if tiers.owner_home_count != mesh.owner_ground_probe_count {
            return Err("owner-home tier count differs from mesh observable".to_owned());
        }
        if mesh.algorithm != FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM
            || mesh.coordinate_encoding != FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING
            || mesh.mesh_sha256 != self.request.mesh_sha256
            || mesh.owner_ground_height_mm != 1_500
        {
            return Err(
                "fixed tier/mesh observable is not bound to the canonical package mesh".to_owned(),
            );
        }
        let total_owner_pairs = tiers
            .owner_home_count
            .checked_mul(tiers.owner_home_count.saturating_sub(1))
            .ok_or_else(|| "owner ground ordered pair arithmetic overflows u64".to_owned())?;
        if mesh
            .blocked_owner_ground_ordered_pair_count
            .checked_add(mesh.open_owner_ground_ordered_pair_count)
            != Some(total_owner_pairs)
        {
            return Err("mesh observable open/blocked pair accounting is invalid".to_owned());
        }
        let expected_point = FIXED_BYTES_V2
            .checked_add(
                OWNER_HOME_BYTES_V2
                    .checked_mul(tiers.owner_home_count)
                    .ok_or_else(|| "owner-home coefficient arithmetic overflows u64".to_owned())?,
            )
            .and_then(|v| v.checked_add(ROUTE_CORE_BYTES_V2.checked_mul(tiers.route_core_count)?))
            .and_then(|v| v.checked_add(TRANSITION_BYTES_V2.checked_mul(tiers.transition_count)?))
            .and_then(|v| v.checked_add(RESIDUAL_BYTES_V2.checked_mul(tiers.residual_count)?))
            .and_then(|v| {
                v.checked_add(
                    OPEN_OWNER_GROUND_PAIR_BYTES_V2
                        .checked_mul(mesh.open_owner_ground_ordered_pair_count)?,
                )
            })
            .ok_or_else(|| "fixed tier/mesh point arithmetic overflows u64".to_owned())?;
        if point != expected_point || self.model.estimated_raw_bytes != expected_point {
            return Err(format!(
                "fixed tier/mesh point_bytes must equal the canonical integer formula ({expected_point})"
            ));
        }
        let expected_low = point
            .checked_mul(70)
            .ok_or_else(|| "fixed tier/mesh lower projection overflows u64".to_owned())?
            / 100;
        let expected_high = point
            .checked_mul(130)
            .and_then(|v| v.checked_add(99))
            .ok_or_else(|| "fixed tier/mesh upper projection overflows u64".to_owned())?
            / 100;
        if calibrated.projected_low_bytes != expected_low
            || calibrated.projected_high_bytes != expected_high
            || calibrated.reservation_bytes != expected_high
            || self.model.projected_low_bytes != expected_low
            || self.model.projected_high_bytes != expected_high
            || self.model.reservation_bytes != expected_high
        {
            return Err("fixed tier/mesh byte band or reservation is not canonical".to_owned());
        }
        if expected_high > MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES {
            return Err("fixed tier/mesh projected high exceeds the mobile hard limit".to_owned());
        }
        Ok(())
    }

    /// Build the additive integer model from canonical pre-bake tier counts and
    /// the package-derived mesh observable.  No floating point participates in
    /// the formula or its ±30% band.
    pub(crate) fn fixed_tier_mesh_model(
        request: &ProbeByteEstimateRequestV2,
        tier_counts: ProbeByteEstimateTierCountsV2,
        mesh_observable: ProbeByteEstimateMeshObservableV2,
    ) -> Result<ProbeByteEstimateModelV2, String> {
        let owner_pairs = tier_counts
            .owner_home_count
            .checked_mul(tier_counts.owner_home_count.saturating_sub(1))
            .ok_or_else(|| "owner ground ordered pair arithmetic overflows u64".to_owned())?;
        if tier_counts.owner_home_count != mesh_observable.owner_ground_probe_count
            || mesh_observable
                .blocked_owner_ground_ordered_pair_count
                .checked_add(mesh_observable.open_owner_ground_ordered_pair_count)
                != Some(owner_pairs)
        {
            return Err(
                "fixed tier/mesh observable does not account for every owner pair".to_owned(),
            );
        }
        let tier_total = tier_counts
            .owner_home_count
            .checked_add(tier_counts.route_core_count)
            .and_then(|value| value.checked_add(tier_counts.transition_count))
            .and_then(|value| value.checked_add(tier_counts.residual_count))
            .ok_or_else(|| "fixed tier count arithmetic overflows u64".to_owned())?;
        if tier_total != request.probe_count {
            return Err("fixed tier counts must sum to request.probe_count".to_owned());
        }
        let point_bytes = FIXED_BYTES_V2
            .checked_add(
                OWNER_HOME_BYTES_V2
                    .checked_mul(tier_counts.owner_home_count)
                    .ok_or_else(|| "owner-home coefficient arithmetic overflows u64".to_owned())?,
            )
            .and_then(|value| {
                value.checked_add(ROUTE_CORE_BYTES_V2.checked_mul(tier_counts.route_core_count)?)
            })
            .and_then(|value| {
                value.checked_add(TRANSITION_BYTES_V2.checked_mul(tier_counts.transition_count)?)
            })
            .and_then(|value| {
                value.checked_add(RESIDUAL_BYTES_V2.checked_mul(tier_counts.residual_count)?)
            })
            .and_then(|value| {
                value.checked_add(
                    OPEN_OWNER_GROUND_PAIR_BYTES_V2
                        .checked_mul(mesh_observable.open_owner_ground_ordered_pair_count)?,
                )
            })
            .ok_or_else(|| "fixed tier/mesh point arithmetic overflows u64".to_owned())?;
        let projected_low_bytes = point_bytes
            .checked_mul(70)
            .map(|value| value / 100)
            .ok_or_else(|| "fixed tier/mesh lower projection overflows u64".to_owned())?;
        let projected_high_bytes = point_bytes
            .checked_mul(130)
            .and_then(|value| value.checked_add(99))
            .map(|value| value / 100)
            .ok_or_else(|| "fixed tier/mesh upper projection overflows u64".to_owned())?;
        Ok(ProbeByteEstimateModelV2 {
            revision: FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION.to_owned(),
            probe_bytes: 0,
            pair_bytes: 0,
            fixed_bytes: 0,
            pair_count: 0,
            pair_count_kind: ProbeByteEstimatePairCountKindV2::Exact,
            estimated_raw_bytes: point_bytes,
            projected_low_bytes,
            projected_high_bytes,
            reservation_bytes: projected_high_bytes,
            calibrated_model: Some(ProbeByteEstimateCalibratedModelV2 {
                revision: FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION.to_owned(),
                fixed_bytes: FIXED_BYTES_V2,
                owner_home_bytes_per_probe: OWNER_HOME_BYTES_V2,
                route_core_bytes_per_probe: ROUTE_CORE_BYTES_V2,
                transition_bytes_per_probe: TRANSITION_BYTES_V2,
                residual_bytes_per_probe: RESIDUAL_BYTES_V2,
                open_owner_ground_ordered_pair_bytes: OPEN_OWNER_GROUND_PAIR_BYTES_V2,
                tier_counts,
                point_bytes,
                projected_low_bytes,
                projected_high_bytes,
                reservation_bytes: projected_high_bytes,
                mesh_observable,
            }),
        })
    }

    /// Compute the domain-bound identity of this envelope's request.
    pub(crate) fn request_sha256(&self) -> Result<String, String> {
        request_sha256(&self.request)
    }
}

/// Compute the canonical request SHA-256.
///
/// Only `request_sha256` itself is omitted from the request object.  The
/// remaining fields retain their typed struct order and are compactly encoded
/// by serde_json, then prefixed by [`REQUEST_SHA256_DOMAIN`].
pub(crate) fn request_sha256(request: &ProbeByteEstimateRequestV2) -> Result<String, String> {
    let canonical = CanonicalRequest::from(request);
    let mut bytes = Vec::with_capacity(REQUEST_SHA256_DOMAIN.len() + 512);
    bytes.extend_from_slice(REQUEST_SHA256_DOMAIN);
    let request_json = serde_json::to_vec(&canonical)
        .map_err(|error| format!("cannot serialize canonical estimate request: {error}"))?;
    bytes.extend_from_slice(&request_json);
    Ok(sha256_hex(&bytes))
}

/// The request object used for hashing, intentionally lacking only its own
/// self-referential hash field.
#[derive(Serialize)]
struct CanonicalRequest<'a> {
    mesh_sha256: &'a str,
    materials_sha256: &'a str,
    probe_plan_sha256: Option<&'a str>,
    placement_policy_sha256: Option<&'a str>,
    probe_layout_sha256: Option<&'a str>,
    probe_count: u64,
    path_horizon_m: u32,
    pathing: &'a ProbeByteEstimatePathingV2,
    sdk: &'a ProbeByteEstimateSdkV2,
}

impl<'a> From<&'a ProbeByteEstimateRequestV2> for CanonicalRequest<'a> {
    fn from(request: &'a ProbeByteEstimateRequestV2) -> Self {
        Self {
            mesh_sha256: &request.mesh_sha256,
            materials_sha256: &request.materials_sha256,
            probe_plan_sha256: request.probe_plan_sha256.as_deref(),
            placement_policy_sha256: request.placement_policy_sha256.as_deref(),
            probe_layout_sha256: request.probe_layout_sha256.as_deref(),
            probe_count: request.probe_count,
            path_horizon_m: request.path_horizon_m,
            pathing: &request.pathing,
            sdk: &request.sdk,
        }
    }
}

fn validate_pathing(pathing: &ProbeByteEstimatePathingV2) -> Result<(), String> {
    finite_positive(
        pathing.visibility_range_m,
        "request.pathing.visibility_range_m",
    )?;
    if pathing.visibility_samples <= 0 {
        return Err("request.pathing.visibility_samples must be greater than zero".to_owned());
    }
    if !pathing.visibility_threshold.is_finite()
        || !(0.0..=1.0).contains(&pathing.visibility_threshold)
    {
        return Err(
            "request.pathing.visibility_threshold must be finite and within 0..=1".to_owned(),
        );
    }
    finite_nonnegative(
        pathing.probe_visibility_radius_m,
        "request.pathing.probe_visibility_radius_m",
    )?;
    if pathing.threads <= 0 {
        return Err("request.pathing.threads must be greater than zero".to_owned());
    }
    Ok(())
}

fn validate_sdk(sdk: &ProbeByteEstimateSdkV2) -> Result<(), String> {
    if sdk.metadata_schema != "fightbox.steam-audio.probe-batch.v1"
        || sdk.steam_audio_version != "4.8.1"
        || sdk.upstream_commit != "0da1825"
        || sdk.baker_revision != "steam-audio-explicit-probes-v1"
    {
        return Err(
            "request.sdk must name the pinned probe-batch schema, Steam Audio build, and explicit baker revision"
                .to_owned(),
        );
    }
    Ok(())
}

fn finite_positive(value: f32, field: &str) -> Result<(), String> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(format!("{field} must be finite and greater than zero"))
    }
}

fn finite_nonnegative(value: f32, field: &str) -> Result<(), String> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(format!("{field} must be finite and non-negative"))
    }
}

fn optional_hash(value: Option<&str>, field: &str) -> Result<(), String> {
    if let Some(value) = value {
        hash(value, field)?;
    }
    Ok(())
}

fn hash(value: &str, field: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{field} must be a lowercase 64-character SHA-256 hex string"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_for(byte: u8) -> String {
        char::from(byte).to_string().repeat(64)
    }

    fn valid_estimate() -> ProbeByteEstimateV2 {
        let mut request = ProbeByteEstimateRequestV2 {
            request_sha256: String::new(),
            mesh_sha256: hash_for(b'a'),
            materials_sha256: hash_for(b'b'),
            probe_plan_sha256: Some(hash_for(b'c')),
            placement_policy_sha256: Some(hash_for(b'd')),
            probe_layout_sha256: Some(hash_for(b'e')),
            probe_count: 100,
            path_horizon_m: 600,
            pathing: ProbeByteEstimatePathingV2 {
                visibility_range_m: 100.0,
                visibility_samples: 6,
                visibility_threshold: 0.5,
                probe_visibility_radius_m: 2.0,
                threads: 1,
            },
            sdk: ProbeByteEstimateSdkV2 {
                metadata_schema: "fightbox.steam-audio.probe-batch.v1".to_owned(),
                steam_audio_version: "4.8.1".to_owned(),
                upstream_commit: "0da1825".to_owned(),
                baker_revision: "steam-audio-explicit-probes-v1".to_owned(),
            },
        };
        request.request_sha256 = request_sha256(&request).expect("request hash");
        let estimated_raw_bytes = 100 * 256 + 200 * 10 + 64 * 1024;
        ProbeByteEstimateV2 {
            schema_version: PROBE_BYTE_ESTIMATE_V2_SCHEMA.to_owned(),
            artifact_state: "completed".to_owned(),
            subject: ProbeByteEstimateSubjectV2 {
                schema_version: "fightbox.city-bake.v2".to_owned(),
                manifest_path: "capabilities/city-bake-v2.json".to_owned(),
                manifest_sha256: hash_for(b'f'),
                package_manifest_sha256: hash_for(b'0'),
            },
            request,
            model: ProbeByteEstimateModelV2 {
                revision: PROVISIONAL_ESTIMATOR_REVISION.to_owned(),
                probe_bytes: 256,
                pair_bytes: 10,
                fixed_bytes: 64 * 1024,
                pair_count: 200,
                pair_count_kind: ProbeByteEstimatePairCountKindV2::Exact,
                estimated_raw_bytes,
                projected_low_bytes: estimated_raw_bytes * 7 / 10,
                projected_high_bytes: (estimated_raw_bytes * 13 + 9) / 10,
                reservation_bytes: (estimated_raw_bytes * 13 + 9) / 10,
                calibrated_model: None,
            },
            observed: ProbeByteEstimateObservedV2 {
                probe_count: 100,
                path_data_size_bytes: 10_000,
                serialized_size_bytes: 20_000,
                payload_sha256: hash_for(b'1'),
                artifact_bytes: 20_100,
            },
        }
    }

    fn valid_fixed_estimate() -> ProbeByteEstimateV2 {
        let mut estimate = valid_estimate();
        estimate.request.placement_policy_sha256 =
            Some(CALIBRATED_MOBILE_PLACEMENT_POLICY_SHA256.to_owned());
        estimate.request.probe_count = 1_826;
        estimate.request.request_sha256 = request_sha256(&estimate.request).unwrap();
        let model = ProbeByteEstimateV2::fixed_tier_mesh_model(
            &estimate.request,
            ProbeByteEstimateTierCountsV2 {
                owner_home_count: 625,
                route_core_count: 700,
                transition_count: 254,
                residual_count: 247,
            },
            ProbeByteEstimateMeshObservableV2 {
                algorithm: FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM.to_owned(),
                coordinate_encoding: FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING.to_owned(),
                mesh_sha256: estimate.request.mesh_sha256.clone(),
                owner_ground_height_mm: 1_500,
                owner_ground_probe_count: 625,
                wall_edge_count: 16,
                blocked_owner_ground_ordered_pair_count: 229_228,
                open_owner_ground_ordered_pair_count: 160_772,
            },
        )
        .unwrap();
        estimate.model = model;
        estimate.observed.probe_count = 1_826;
        estimate.observed.artifact_bytes = 4_000_000;
        estimate
    }

    #[test]
    fn fixed_model_roundtrip_and_integer_accounting_are_canonical() {
        let estimate = valid_fixed_estimate();
        estimate.validate().unwrap();
        let calibrated = estimate.model.calibrated_model.as_ref().unwrap();
        assert_eq!(calibrated.point_bytes, 4_371_509);
        assert_eq!(estimate.model.projected_low_bytes, 3_060_056);
        assert_eq!(estimate.model.projected_high_bytes, 5_682_962);
        let bytes = estimate.to_canonical_json().unwrap();
        assert_eq!(ProbeByteEstimateV2::from_json(&bytes).unwrap(), estimate);
    }

    #[test]
    fn fixed_model_rejects_tampered_tier_mesh_and_model_fields() {
        let mut tampered = valid_fixed_estimate();
        tampered
            .model
            .calibrated_model
            .as_mut()
            .unwrap()
            .tier_counts
            .route_core_count += 1;
        assert!(
            tampered.validate().is_err(),
            "tier count must be cross-bound"
        );

        let mut tampered = valid_fixed_estimate();
        tampered
            .model
            .calibrated_model
            .as_mut()
            .unwrap()
            .mesh_observable
            .mesh_sha256 = hash_for(b'z');
        assert!(
            tampered.validate().is_err(),
            "mesh identity must be cross-bound"
        );

        let mut tampered = valid_fixed_estimate();
        tampered
            .model
            .calibrated_model
            .as_mut()
            .unwrap()
            .point_bytes += 1;
        assert!(
            tampered.validate().is_err(),
            "model coefficients must be frozen"
        );

        let mut tampered = valid_fixed_estimate();
        tampered.model.revision = "reachable-ordered-pairs-p256-q10-fixed64k-v1+silent".to_owned();
        assert!(
            tampered.validate().is_err(),
            "unknown revision must not fall back"
        );
    }

    #[test]
    fn fixed_model_rejects_overflow_and_mobile_over_hard_admission() {
        let mut estimate = valid_fixed_estimate();
        let tiers = ProbeByteEstimateTierCountsV2 {
            owner_home_count: u64::MAX,
            route_core_count: 0,
            transition_count: 0,
            residual_count: 0,
        };
        let mesh = ProbeByteEstimateMeshObservableV2 {
            algorithm: FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM.to_owned(),
            coordinate_encoding: FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING.to_owned(),
            mesh_sha256: estimate.request.mesh_sha256.clone(),
            owner_ground_height_mm: 1_500,
            owner_ground_probe_count: u64::MAX,
            wall_edge_count: 0,
            blocked_owner_ground_ordered_pair_count: 0,
            open_owner_ground_ordered_pair_count: u64::MAX,
        };
        assert!(
            ProbeByteEstimateV2::fixed_tier_mesh_model(&estimate.request, tiers, mesh).is_err()
        );

        estimate.request.probe_count = 20_003;
        estimate.request.request_sha256 = request_sha256(&estimate.request).unwrap();
        let model = ProbeByteEstimateV2::fixed_tier_mesh_model(
            &estimate.request,
            ProbeByteEstimateTierCountsV2 {
                owner_home_count: 20_000,
                route_core_count: 1,
                transition_count: 1,
                residual_count: 1,
            },
            ProbeByteEstimateMeshObservableV2 {
                algorithm: FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM.to_owned(),
                coordinate_encoding: FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING.to_owned(),
                mesh_sha256: estimate.request.mesh_sha256.clone(),
                owner_ground_height_mm: 1_500,
                owner_ground_probe_count: 20_000,
                wall_edge_count: 1,
                blocked_owner_ground_ordered_pair_count: 20_000 * 19_999,
                open_owner_ground_ordered_pair_count: 0,
            },
        )
        .unwrap();
        estimate.model = model;
        estimate.observed.probe_count = 20_003;
        assert!(estimate.validate().unwrap_err().contains("hard limit"));
    }

    #[test]
    fn fixed_model_allows_zero_named_tiers_and_zero_owner_pairs() {
        let mut estimate = valid_estimate();
        estimate.request.placement_policy_sha256 =
            Some(CALIBRATED_MOBILE_PLACEMENT_POLICY_SHA256.to_owned());
        estimate.request.request_sha256 = request_sha256(&estimate.request).unwrap();
        estimate.model = ProbeByteEstimateV2::fixed_tier_mesh_model(
            &estimate.request,
            ProbeByteEstimateTierCountsV2 {
                owner_home_count: 0,
                route_core_count: 0,
                transition_count: 0,
                residual_count: 100,
            },
            ProbeByteEstimateMeshObservableV2 {
                algorithm: FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM.to_owned(),
                coordinate_encoding: FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING.to_owned(),
                mesh_sha256: estimate.request.mesh_sha256.clone(),
                owner_ground_height_mm: 1_500,
                owner_ground_probe_count: 0,
                wall_edge_count: 0,
                blocked_owner_ground_ordered_pair_count: 0,
                open_owner_ground_ordered_pair_count: 0,
            },
        )
        .unwrap();
        estimate.observed.path_data_size_bytes = 4_000;
        estimate.observed.serialized_size_bytes = 8_000;
        estimate.observed.artifact_bytes = 10_000;
        estimate.validate().unwrap();
    }

    #[test]
    fn fixed_model_rejects_observed_artifact_outside_exact_band() {
        let mut estimate = valid_fixed_estimate();
        estimate.observed.artifact_bytes = estimate.model.projected_low_bytes - 1;
        let error = estimate.validate().unwrap_err();
        assert!(error.contains("within projected_low_bytes"), "{error}");
        estimate.observed.artifact_bytes = estimate.model.projected_high_bytes + 1;
        let error = estimate.validate().unwrap_err();
        assert!(error.contains("within projected_low_bytes"), "{error}");
    }

    #[test]
    fn fixed_model_reservation_and_envelope_self_size_are_exact() {
        let (estimate, bytes) = valid_fixed_estimate()
            .finalize_canonical_json(4_000_000)
            .unwrap();
        assert_eq!(
            estimate.observed.artifact_bytes,
            4_000_000 + bytes.len() as u64
        );
        assert_eq!(
            estimate.model.reservation_bytes,
            estimate.model.projected_high_bytes
        );
        assert!(estimate.observed.artifact_bytes <= estimate.model.reservation_bytes);
        assert_eq!(ProbeByteEstimateV2::from_json(&bytes).unwrap(), estimate);
    }

    #[test]
    fn canonical_roundtrip_preserves_the_typed_envelope() {
        let estimate = valid_estimate();
        let bytes = estimate.to_canonical_json().expect("canonical JSON");
        assert_eq!(bytes, estimate.to_canonical_json().expect("stable JSON"));
        let decoded = ProbeByteEstimateV2::from_json(&bytes).expect("roundtrip");
        assert_eq!(decoded, estimate);
    }

    #[test]
    fn final_envelope_counts_its_own_exact_canonical_bytes() {
        let base_bytes = 25_000;
        let (estimate, bytes) = valid_estimate()
            .finalize_canonical_json(base_bytes)
            .expect("fixed-point envelope");
        assert_eq!(
            estimate.observed.artifact_bytes,
            base_bytes + u64::try_from(bytes.len()).unwrap()
        );
        assert_eq!(ProbeByteEstimateV2::from_json(&bytes).unwrap(), estimate);
    }

    #[test]
    fn every_envelope_layer_rejects_unknown_fields() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&valid_estimate().to_canonical_json().unwrap()).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(ProbeByteEstimateV2::from_json(&serde_json::to_vec(&value).unwrap()).is_err());

        let mut value: serde_json::Value =
            serde_json::from_slice(&valid_estimate().to_canonical_json().unwrap()).unwrap();
        value["request"]["pathing"]["unknown"] = serde_json::json!(true);
        assert!(ProbeByteEstimateV2::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn inflated_reservation_and_rehashed_unpinned_sdk_are_rejected() {
        let mut estimate = valid_estimate();
        estimate.model.reservation_bytes += 1;
        let error = estimate
            .validate()
            .expect_err("inflated reservation must fail");
        assert!(error.contains("must equal"), "{error}");

        let mut estimate = valid_estimate();
        estimate.request.sdk.baker_revision = "untrusted-explicit-baker".to_owned();
        estimate.request.request_sha256 = request_sha256(&estimate.request).unwrap();
        let error = estimate
            .validate()
            .expect_err("unpinned SDK identity must fail");
        assert!(error.contains("pinned"), "{error}");
    }

    #[test]
    fn request_mutation_breaks_the_domain_bound_identity() {
        let mut estimate = valid_estimate();
        estimate.request.path_horizon_m += 1;
        let error = estimate.validate().expect_err("mutated request must fail");
        assert!(error.contains("request.request_sha256"), "{error}");
        assert_ne!(
            estimate.request.request_sha256,
            estimate.request_sha256().unwrap()
        );
    }
}
