//! Explicit-position probe baking for globally planned city cells.
//!
//! This is deliberately separate from the legacy uniform-floor S3 baker. The
//! caller supplies an already ordered list of ENU probe spheres; this module
//! validates it and the linked implementation adds those spheres to one Steam
//! Audio probe batch in exactly that order. It never snaps, sorts, deduplicates,
//! or regenerates planner output.

use crate::{BackendError, BakedProbeBatch, EnuVector3, PathBakeConfig, SceneMesh};

/// Implementation revision persisted by the city-bake-v2 sidecar.
pub const EXPLICIT_PROBE_BAKER_REVISION: &str = "steam-audio-explicit-probes-v1";

/// One caller-authored Steam Audio influence sphere in local ENU metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExplicitProbe {
    pub center_enu_m: EnuVector3,
    pub radius_m: f32,
}

impl ExplicitProbe {
    #[must_use]
    pub const fn new(center_enu_m: EnuVector3, radius_m: f32) -> Self {
        Self {
            center_enu_m,
            radius_m,
        }
    }
}

/// Owned request for one explicit-position pathing bake.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplicitProbeBakeRequest {
    pub mesh: SceneMesh,
    /// Ordered probe spheres. Their order is part of the caller's layout hash.
    pub probes: Vec<ExplicitProbe>,
    pub pathing: PathBakeConfig,
}

/// Bakes one pathing layer over the exact explicit probe sequence.
///
/// No uniform-floor probe array is created. A successful return therefore
/// proves that the serialized batch was constructed from `request.probes`.
pub fn bake_explicit_probe_batch(
    request: &ExplicitProbeBakeRequest,
) -> Result<BakedProbeBatch, BackendError> {
    validate_explicit_probes(&request.probes)?;
    #[cfg(feature = "linked-sdk")]
    {
        let baked = crate::linked::bake_explicit_probe_batch(request)?;
        verify_serialized_probe_sequence(&baked, &request.probes)?;
        Ok(baked)
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        let _ = request;
        Err(BackendError::SdkUnavailable(crate::unavailable_metadata()))
    }
}

#[cfg(feature = "linked-sdk")]
fn verify_serialized_probe_sequence(
    baked: &BakedProbeBatch,
    expected: &[ExplicitProbe],
) -> Result<(), BackendError> {
    let coverage = baked.probe_coverage()?;
    if coverage.probe_count() != expected.len() {
        return Err(BackendError::InvalidProbeBatch(
            "serialized explicit probe count differs from the submitted sequence",
        ));
    }
    for ((actual_center, actual_radius), expected) in coverage.spheres().zip(expected) {
        if actual_center.x.to_bits() != expected.center_enu_m.x.to_bits()
            || actual_center.y.to_bits() != expected.center_enu_m.y.to_bits()
            || actual_center.z.to_bits() != expected.center_enu_m.z.to_bits()
            || actual_radius.to_bits() != expected.radius_m.to_bits()
        {
            return Err(BackendError::InvalidProbeBatch(
                "serialized explicit probe sequence differs from the submitted ENU spheres",
            ));
        }
    }
    Ok(())
}

fn validate_explicit_probes(probes: &[ExplicitProbe]) -> Result<(), BackendError> {
    if probes.is_empty() {
        return Err(BackendError::InvalidInput(
            "explicit probe list must not be empty",
        ));
    }
    if probes.len() > i32::MAX as usize {
        return Err(BackendError::InvalidInput(
            "explicit probe count exceeds Steam Audio's signed 32-bit limit",
        ));
    }
    if probes.iter().any(|probe| {
        !probe.center_enu_m.x.is_finite()
            || !probe.center_enu_m.y.is_finite()
            || !probe.center_enu_m.z.is_finite()
            || !probe.radius_m.is_finite()
            || probe.radius_m <= 0.0
    }) {
        return Err(BackendError::InvalidInput(
            "explicit probe centres must be finite and radii must be finite and positive",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_explicit_sites_fail_before_sdk_dispatch() {
        assert!(matches!(
            validate_explicit_probes(&[]),
            Err(BackendError::InvalidInput(
                "explicit probe list must not be empty"
            ))
        ));
        assert!(
            validate_explicit_probes(&[ExplicitProbe::new(EnuVector3::new(1.0, 2.0, 3.0), 0.0,)])
                .is_err()
        );
        assert!(
            validate_explicit_probes(&[ExplicitProbe::new(
                EnuVector3::new(f32::NAN, 2.0, 3.0),
                4.0,
            )])
            .is_err()
        );
    }

    #[test]
    fn valid_sites_preserve_caller_order_and_exact_f32_values() {
        let probes = vec![
            ExplicitProbe::new(EnuVector3::new(-0.125, 4.5, 63.0), 16.0),
            ExplicitProbe::new(EnuVector3::new(8.0, -32.25, 1.5), 8.0),
        ];
        validate_explicit_probes(&probes).unwrap();
        assert_eq!(probes[0].center_enu_m.x.to_bits(), (-0.125_f32).to_bits());
        assert_eq!(probes[1].center_enu_m.y.to_bits(), (-32.25_f32).to_bits());
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn linked_tiny_batch_commits_the_exact_explicit_count() {
        let baked = bake_explicit_probe_batch(&ExplicitProbeBakeRequest {
            mesh: SceneMesh::controlled_s3_corner(),
            probes: vec![
                ExplicitProbe::new(EnuVector3::new(-5.0, -5.0, 1.5), 8.0),
                ExplicitProbe::new(EnuVector3::new(5.0, 5.0, 1.5), 8.0),
            ],
            pathing: PathBakeConfig {
                path_range_m: 20.0,
                visibility_range_m: 20.0,
                ..PathBakeConfig::default()
            },
        })
        .expect("tiny explicit batch should bake");
        baked.validate().unwrap();
        assert_eq!(baked.metadata.probe_count, 2);
    }
}
