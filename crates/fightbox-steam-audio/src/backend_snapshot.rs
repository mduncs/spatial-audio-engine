//! Backend-private, fixed-capacity Steam Audio publication layout.

use crate::width_render::{DECLARED_LATENCY_SAMPLES, WIDTH_RENDERER_REVISION};
use crate::{BackendError, EnuVector3, SteamVector3, enu_to_steam};
use fightbox_api::{EnuVector3 as ApiEnuVector3, ExtentDescriptor};
use fightbox_runtime::backend::MAX_ACTIVE_SOURCES;

pub(crate) const MAX_PATH_SH_COEFFS: usize = 16;
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SteamDirectParams {
    pub(crate) distance_attenuation: f32,
    pub(crate) air_absorption: [f32; 3],
    pub(crate) directivity: f32,
    pub(crate) occlusion: f32,
    pub(crate) transmission: [f32; 3],
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SteamReflectionParams {
    pub(crate) ir: usize,
    pub(crate) reverb_times: [f32; 3],
    pub(crate) eq: [f32; 3],
    pub(crate) delay: i32,
    pub(crate) num_channels: i32,
    pub(crate) ir_size: i32,
    pub(crate) tan_slot: i32,
}

/// Additive Wave 11 state for diagnostics and future workbench publication.
///
/// In v1 only `LineSegment` has non-zero geometric and phase controls. Keeping
/// the original descriptor here makes the structural bypass for all other
/// extents observable without changing the frozen runtime seam.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SteamWidthState {
    pub(crate) descriptor: ExtentDescriptor,
    pub(crate) geometric_k: f32,
    pub(crate) phi_eff_radians: f32,
    pub(crate) declared_latency_samples: u32,
    pub(crate) renderer_revision: &'static str,
}

impl Default for SteamWidthState {
    fn default() -> Self {
        Self {
            descriptor: ExtentDescriptor::Point,
            geometric_k: 0.0,
            phi_eff_radians: 0.0,
            declared_latency_samples: DECLARED_LATENCY_SAMPLES,
            renderer_revision: WIDTH_RENDERER_REVISION,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SteamSourcePropagation {
    pub(crate) active: bool,
    pub(crate) source_position: SteamVector3,
    pub(crate) source_forward: SteamVector3,
    pub(crate) source_up: SteamVector3,
    pub(crate) linear_velocity_mps: SteamVector3,
    pub(crate) width: SteamWidthState,
    pub(crate) direct: SteamDirectParams,
    pub(crate) path_eq: [f32; 3],
    pub(crate) path_sh: [f32; MAX_PATH_SH_COEFFS],
    pub(crate) configured_pathing_order: u8,
    pub(crate) reflections: SteamReflectionParams,
}

impl Default for SteamSourcePropagation {
    fn default() -> Self {
        Self {
            active: false,
            source_position: SteamVector3::default(),
            source_forward: SteamVector3::new(0.0, 0.0, -1.0),
            source_up: SteamVector3::new(0.0, 1.0, 0.0),
            linear_velocity_mps: SteamVector3::default(),
            width: SteamWidthState::default(),
            direct: SteamDirectParams {
                distance_attenuation: 1.0,
                air_absorption: [1.0; 3],
                directivity: 1.0,
                occlusion: 1.0,
                transmission: [1.0; 3],
            },
            path_eq: [0.0; 3],
            path_sh: [0.0; MAX_PATH_SH_COEFFS],
            configured_pathing_order: 0,
            reflections: SteamReflectionParams::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SteamPropagationSnapshot {
    pub(crate) world_generation: u64,
    pub(crate) sequence: u64,
    pub(crate) simulated_at_ns: u64,
    pub(crate) listener_position: SteamVector3,
    pub(crate) listener_linear_velocity_mps: SteamVector3,
    pub(crate) sources: [SteamSourcePropagation; MAX_ACTIVE_SOURCES],
}

impl Default for SteamPropagationSnapshot {
    fn default() -> Self {
        Self {
            world_generation: 0,
            sequence: 0,
            simulated_at_ns: 0,
            listener_position: SteamVector3::default(),
            listener_linear_velocity_mps: SteamVector3::default(),
            sources: [SteamSourcePropagation::default(); MAX_ACTIVE_SOURCES],
        }
    }
}

pub(crate) fn path_coefficient_count(order: i32) -> Option<usize> {
    if !(0..=3).contains(&order) {
        return None;
    }
    usize::try_from((order + 1) * (order + 1))
        .ok()
        .filter(|count| *count <= MAX_PATH_SH_COEFFS)
}

pub(crate) fn fixed_path_sh(
    configured_order: i32,
    coefficients: &[f32],
) -> Result<[f32; MAX_PATH_SH_COEFFS], BackendError> {
    let count = path_coefficient_count(configured_order).ok_or(BackendError::InvalidInput(
        "pathing order must be between zero and three",
    ))?;
    if coefficients.len() != count {
        return Err(BackendError::InvalidSdkOutput(
            "path SH coefficient count does not match configured order",
        ));
    }
    let mut fixed = [0.0; MAX_PATH_SH_COEFFS];
    fixed[..count].copy_from_slice(coefficients);
    Ok(fixed)
}

pub(crate) fn api_enu_to_steam(vector: ApiEnuVector3) -> SteamVector3 {
    enu_to_steam(EnuVector3::new(vector.east_m, vector.north_m, vector.up_m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_coordinate_conversion_uses_the_single_enu_mapping() {
        assert_eq!(
            api_enu_to_steam(ApiEnuVector3::new(2.0, 3.0, 5.0)),
            SteamVector3::new(2.0, 5.0, -3.0)
        );
    }

    #[test]
    fn configured_order_copies_exact_coefficients_and_zeroes_tail() {
        let fixed = fixed_path_sh(1, &[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(&fixed[..4], &[1.0, 2.0, 3.0, 4.0]);
        assert!(fixed[4..].iter().all(|value| *value == 0.0));
    }

    #[test]
    fn snapshot_layout_has_fixed_source_and_max_order_capacity() {
        let snapshot = SteamPropagationSnapshot::default();
        assert_eq!(snapshot.sources.len(), MAX_ACTIVE_SOURCES);
        assert_eq!(snapshot.sources[0].path_sh.len(), MAX_PATH_SH_COEFFS);
        assert_eq!(path_coefficient_count(3), Some(MAX_PATH_SH_COEFFS));
        assert_eq!(
            snapshot.sources[0].width.descriptor,
            ExtentDescriptor::Point
        );
        assert_eq!(
            snapshot.sources[0].width.geometric_k.to_bits(),
            0.0_f32.to_bits()
        );
        assert_eq!(
            snapshot.sources[0].width.phi_eff_radians.to_bits(),
            0.0_f32.to_bits()
        );
        assert_eq!(snapshot.sources[0].width.declared_latency_samples, 0);
        assert_eq!(
            snapshot.sources[0].width.renderer_revision,
            WIDTH_RENDERER_REVISION
        );
    }
}
