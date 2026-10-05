//! Authored V1 acoustic zones and static portals.
//!
//! These controls describe enclosure coloration and one shared late field. They
//! do not synthesize coherent doorway paths, infer rooms, or animate doors.

use crate::{EnuVector3, diffuse::DiffuseFieldProfile, spectral::SPECTRAL_BAND_COUNT};

/// Reserved identity for the unbounded outdoor environment.
pub const EXTERIOR_ZONE_ID: AcousticZoneId = AcousticZoneId(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AcousticZoneId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StaticPortalId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcousticZoneKind {
    Exterior,
    Interior,
    SemiOpen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnclosureProvenance {
    AuthoredStatic,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisAlignedZoneBounds {
    pub minimum: EnuVector3,
    pub maximum: EnuVector3,
}

impl AxisAlignedZoneBounds {
    pub fn validate(self) -> Result<(), EnclosureAuthoringError> {
        if !self.minimum.is_finite() || !self.maximum.is_finite() {
            return Err(EnclosureAuthoringError::NonFiniteGeometry);
        }
        if self.minimum.east_m >= self.maximum.east_m
            || self.minimum.north_m >= self.maximum.north_m
            || self.minimum.up_m >= self.maximum.up_m
        {
            return Err(EnclosureAuthoringError::EmptyZoneBounds);
        }
        Ok(())
    }
}

/// One explicitly authored volume. Higher priority wins when volumes overlap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AcousticZone {
    pub id: AcousticZoneId,
    pub kind: AcousticZoneKind,
    pub bounds: AxisAlignedZoneBounds,
    pub priority: u16,
    /// Inward distance over which boundary coloration and late energy fade in.
    pub transition_depth_m: f32,
    /// Attenuation incurred when a path crosses this zone's solid boundary.
    pub boundary_gain_db: [f32; SPECTRAL_BAND_COUNT],
    pub diffuse_field: DiffuseFieldProfile,
    pub provenance: EnclosureProvenance,
}

impl AcousticZone {
    pub fn validate(self) -> Result<(), EnclosureAuthoringError> {
        if self.id == EXTERIOR_ZONE_ID {
            return Err(EnclosureAuthoringError::ReservedZoneId);
        }
        self.bounds.validate()?;
        if !self.transition_depth_m.is_finite() || self.transition_depth_m < 0.0 {
            return Err(EnclosureAuthoringError::InvalidTransitionDepth);
        }
        validate_attenuation(self.boundary_gain_db)?;
        self.diffuse_field
            .validate()
            .map_err(|_| EnclosureAuthoringError::InvalidDiffuseField)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaticPortalState {
    Open,
    Closed,
}

/// A rectangular, static aperture connecting two authored environments.
///
/// `normal` and `up` must be unit length and perpendicular. The portal's right
/// axis is derived deterministically as `normal × up`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StaticPortal {
    pub id: StaticPortalId,
    pub zone_a: AcousticZoneId,
    pub zone_b: AcousticZoneId,
    pub center: EnuVector3,
    pub normal: EnuVector3,
    pub up: EnuVector3,
    pub half_width_m: f32,
    pub half_height_m: f32,
    /// Soft edge inside the aperture. This is a gain blend, never a delayed path.
    pub edge_transition_m: f32,
    pub state: StaticPortalState,
    pub open_gain_db: [f32; SPECTRAL_BAND_COUNT],
    pub closed_gain_db: [f32; SPECTRAL_BAND_COUNT],
    pub provenance: EnclosureProvenance,
}

impl StaticPortal {
    pub fn validate(self) -> Result<(), EnclosureAuthoringError> {
        if self.zone_a == self.zone_b {
            return Err(EnclosureAuthoringError::PortalConnectsSameZone);
        }
        if !self.center.is_finite() || !self.normal.is_finite() || !self.up.is_finite() {
            return Err(EnclosureAuthoringError::NonFiniteGeometry);
        }
        let normal_length = vector_length(self.normal);
        let up_length = vector_length(self.up);
        let dot = vector_dot(self.normal, self.up).abs();
        if (normal_length - 1.0).abs() > 1.0e-3 || (up_length - 1.0).abs() > 1.0e-3 || dot > 1.0e-3
        {
            return Err(EnclosureAuthoringError::InvalidPortalAxes);
        }
        if !self.half_width_m.is_finite()
            || !self.half_height_m.is_finite()
            || self.half_width_m <= 0.0
            || self.half_height_m <= 0.0
        {
            return Err(EnclosureAuthoringError::InvalidPortalExtent);
        }
        if !self.edge_transition_m.is_finite() || self.edge_transition_m < 0.0 {
            return Err(EnclosureAuthoringError::InvalidTransitionDepth);
        }
        validate_attenuation(self.open_gain_db)?;
        validate_attenuation(self.closed_gain_db)
    }
}

fn validate_attenuation(
    gain_db: [f32; SPECTRAL_BAND_COUNT],
) -> Result<(), EnclosureAuthoringError> {
    if gain_db
        .into_iter()
        .any(|gain| !gain.is_finite() || !(-120.0..=0.0).contains(&gain))
    {
        return Err(EnclosureAuthoringError::InvalidBoundaryGain);
    }
    Ok(())
}

fn vector_length(vector: EnuVector3) -> f32 {
    vector_dot(vector, vector).sqrt()
}

fn vector_dot(left: EnuVector3, right: EnuVector3) -> f32 {
    left.east_m * right.east_m + left.north_m * right.north_m + left.up_m * right.up_m
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnclosureAuthoringError {
    ReservedZoneId,
    DuplicateZoneId,
    DuplicatePortalId,
    UnknownPortalZone,
    EmptyZoneBounds,
    NonFiniteGeometry,
    InvalidTransitionDepth,
    InvalidBoundaryGain,
    InvalidDiffuseField,
    PortalConnectsSameZone,
    InvalidPortalAxes,
    InvalidPortalExtent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_zone_rejects_a_reserved_identity() {
        let zone = AcousticZone {
            id: EXTERIOR_ZONE_ID,
            kind: AcousticZoneKind::Interior,
            bounds: AxisAlignedZoneBounds {
                minimum: EnuVector3::new(0.0, 0.0, 0.0),
                maximum: EnuVector3::new(10.0, 10.0, 3.0),
            },
            priority: 0,
            transition_depth_m: 1.0,
            boundary_gain_db: [-6.0; SPECTRAL_BAND_COUNT],
            diffuse_field: DiffuseFieldProfile::SMALL_INTERIOR,
            provenance: EnclosureProvenance::AuthoredStatic,
        };
        assert_eq!(
            zone.validate(),
            Err(EnclosureAuthoringError::ReservedZoneId)
        );
    }
}
