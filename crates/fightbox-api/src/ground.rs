//! Authored inputs for V1 statistical ground coloration.
//!
//! This contract describes a smooth octave correction, not a coherent delayed
//! ground reflection. It never introduces another propagation path or comb
//! filter and it cannot share authority with diffraction on the same segment.

/// Semantic source class used by the default ground policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroundSourceKind {
    #[default]
    SteadyGroundBound,
    Airborne,
    BallisticCrack,
    Thunder,
    Firework,
    Explosion,
}

/// Explicit authoring control for exceptional assets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroundAuthoringPolicy {
    #[default]
    Default,
    ForceOff,
    /// Applies the smooth statistical correction to an otherwise excluded
    /// source class. Evidence must retain this non-normative provenance.
    ForceOnNonNormative,
}

/// One physical segment considered for statistical ground coloration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatisticalGroundRequest {
    /// Authored ISO/CNOSSOS-style ground factor: 0 is hard, 1 is porous.
    pub ground_factor: f32,
    pub distance_m: f32,
    pub source_height_m: f32,
    pub listener_height_m: f32,
    pub source_kind: GroundSourceKind,
    /// False when geometry has already established an obstructed path.
    pub unobstructed: bool,
    /// True when diffraction owns this segment. Ground must then remain off.
    pub diffraction_applied: bool,
    pub authoring: GroundAuthoringPolicy,
}

impl StatisticalGroundRequest {
    #[must_use]
    pub const fn steady_ground_bound(
        ground_factor: f32,
        distance_m: f32,
        source_height_m: f32,
        listener_height_m: f32,
    ) -> Self {
        Self {
            ground_factor,
            distance_m,
            source_height_m,
            listener_height_m,
            source_kind: GroundSourceKind::SteadyGroundBound,
            unobstructed: true,
            diffraction_applied: false,
            authoring: GroundAuthoringPolicy::Default,
        }
    }

    pub fn validate(self) -> Result<(), StatisticalGroundRequestError> {
        if !self.ground_factor.is_finite() {
            return Err(StatisticalGroundRequestError::NonFiniteGroundFactor);
        }
        if !(0.0..=1.0).contains(&self.ground_factor) {
            return Err(StatisticalGroundRequestError::GroundFactorOutOfRange);
        }
        if !self.distance_m.is_finite() || self.distance_m < 0.0 {
            return Err(StatisticalGroundRequestError::InvalidDistance);
        }
        if !self.source_height_m.is_finite() || self.source_height_m < 0.0 {
            return Err(StatisticalGroundRequestError::InvalidSourceHeight);
        }
        if !self.listener_height_m.is_finite() || self.listener_height_m < 0.0 {
            return Err(StatisticalGroundRequestError::InvalidListenerHeight);
        }
        Ok(())
    }
}

/// Invalid statistical-ground input rejected before transfer composition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatisticalGroundRequestError {
    NonFiniteGroundFactor,
    GroundFactorOutOfRange,
    InvalidDistance,
    InvalidSourceHeight,
    InvalidListenerHeight,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_ground_factor_and_geometry_have_closed_valid_ranges() {
        for factor in [0.0, 0.5, 1.0] {
            assert_eq!(
                StatisticalGroundRequest::steady_ground_bound(factor, 100.0, 0.5, 1.5).validate(),
                Ok(())
            );
        }
        assert_eq!(
            StatisticalGroundRequest::steady_ground_bound(1.01, 100.0, 0.5, 1.5).validate(),
            Err(StatisticalGroundRequestError::GroundFactorOutOfRange)
        );
        assert_eq!(
            StatisticalGroundRequest::steady_ground_bound(0.5, f32::NAN, 0.5, 1.5).validate(),
            Err(StatisticalGroundRequestError::InvalidDistance)
        );
    }
}
