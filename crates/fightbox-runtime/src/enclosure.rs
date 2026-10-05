//! Control-rate evaluation of authored acoustic zones and static portals.

use fightbox_api::{
    EnuVector3,
    diffuse::DiffuseFieldProfile,
    enclosure::{
        AcousticZone, AcousticZoneId, EXTERIOR_ZONE_ID, EnclosureAuthoringError, StaticPortal,
        StaticPortalId, StaticPortalState,
    },
    spectral::{SPECTRAL_BAND_COUNT, SpectralStage, SpectralTransfer, SpectralTransferError},
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnclosureAuthority {
    Exterior,
    SameZone,
    ZoneBoundary,
    OpenPortal(StaticPortalId),
    ClosedPortal(StaticPortalId),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnclosureEvaluation {
    pub listener_zone_id: AcousticZoneId,
    pub source_zone_id: AcousticZoneId,
    pub listener_zone_weight: f32,
    pub enclosure_gain_db: [f32; SPECTRAL_BAND_COUNT],
    pub diffuse_field: DiffuseFieldProfile,
    pub authority: EnclosureAuthority,
}

impl EnclosureEvaluation {
    /// Publishes the enclosure contribution into the one composed spectral path.
    pub fn publish(self, transfer: &mut SpectralTransfer) -> Result<(), SpectralTransferError> {
        transfer.set_stage(SpectralStage::Enclosure, self.enclosure_gain_db)
    }
}

/// Immutable authored enclosure scene evaluated off the audio callback.
#[derive(Clone, Debug)]
pub struct EnclosureScene {
    zones: Vec<AcousticZone>,
    portals: Vec<StaticPortal>,
    /// Portals grouped by their unordered `(zone_a, zone_b)` pair as indices
    /// into `portals` (always in-bounds: built once below from that same vec,
    /// which is immutable afterwards). Each list keeps ascending portal-id
    /// order matching the sorted store, so evaluation visits only portals
    /// connecting the sampled path in the same order a full scan would.
    portals_by_zone_pair: HashMap<(AcousticZoneId, AcousticZoneId), Vec<usize>>,
}

impl EnclosureScene {
    pub fn new(
        mut zones: Vec<AcousticZone>,
        mut portals: Vec<StaticPortal>,
    ) -> Result<Self, EnclosureAuthoringError> {
        for zone in &zones {
            zone.validate()?;
        }
        zones.sort_by_key(|zone| zone.id);
        if zones.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(EnclosureAuthoringError::DuplicateZoneId);
        }

        for portal in &portals {
            portal.validate()?;
            for zone_id in [portal.zone_a, portal.zone_b] {
                if zone_id != EXTERIOR_ZONE_ID
                    && zones
                        .binary_search_by_key(&zone_id, |zone| zone.id)
                        .is_err()
                {
                    return Err(EnclosureAuthoringError::UnknownPortalZone);
                }
            }
        }
        portals.sort_by_key(|portal| portal.id);
        if portals.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(EnclosureAuthoringError::DuplicatePortalId);
        }
        let mut portals_by_zone_pair: HashMap<(AcousticZoneId, AcousticZoneId), Vec<usize>> =
            HashMap::new();
        for (index, portal) in portals.iter().enumerate() {
            portals_by_zone_pair
                .entry(zone_pair_key(portal.zone_a, portal.zone_b))
                .or_default()
                .push(index);
        }
        Ok(Self {
            zones,
            portals,
            portals_by_zone_pair,
        })
    }

    #[must_use]
    pub fn zones(&self) -> &[AcousticZone] {
        &self.zones
    }

    #[must_use]
    pub fn portals(&self) -> &[StaticPortal] {
        &self.portals
    }

    /// Evaluates one source-to-listener path. Geometry uses `f64` internally so
    /// a city-scale ENU origin does not need to be moved onto the audio thread.
    pub fn evaluate(
        &self,
        listener: EnuVector3,
        source: EnuVector3,
        pose_uncertainty_m: f32,
    ) -> Result<EnclosureEvaluation, EnclosureEvaluationError> {
        if !listener.is_finite() || !source.is_finite() {
            return Err(EnclosureEvaluationError::NonFinitePosition);
        }
        if !pose_uncertainty_m.is_finite() || pose_uncertainty_m < 0.0 {
            return Err(EnclosureEvaluationError::InvalidPoseUncertainty);
        }

        let listener_sample = self.sample_zone(listener, pose_uncertainty_m);
        let source_sample = self.sample_zone(source, pose_uncertainty_m);
        let diffuse_field = listener_sample
            .zone
            .map_or(DiffuseFieldProfile::OFF, |zone| {
                scale_diffuse(zone.diffuse_field, listener_sample.weight)
            });

        let same_zone_boundary_transition = listener_sample.id == source_sample.id
            && listener_sample.id != EXTERIOR_ZONE_ID
            && (listener_sample.weight - source_sample.weight).abs() > f32::EPSILON;
        if listener_sample.id == source_sample.id && !same_zone_boundary_transition {
            return Ok(EnclosureEvaluation {
                listener_zone_id: listener_sample.id,
                source_zone_id: source_sample.id,
                listener_zone_weight: listener_sample.weight,
                enclosure_gain_db: [0.0; SPECTRAL_BAND_COUNT],
                diffuse_field,
                authority: if listener_sample.id == EXTERIOR_ZONE_ID {
                    EnclosureAuthority::Exterior
                } else {
                    EnclosureAuthority::SameZone
                },
            });
        }

        let mut path_listener_id = listener_sample.id;
        let mut path_source_id = source_sample.id;
        let mut portal_extension_m = 0.0_f64;
        let mut boundary_gain_db = [0.0; SPECTRAL_BAND_COUNT];
        let crossing_weight;
        if same_zone_boundary_transition {
            // The shallower point is still in the authored inward transition
            // band. Treat its remaining boundary exposure as an exterior leg
            // so the same-zone decision cannot snap to neutral at the plane.
            crossing_weight = (listener_sample.weight - source_sample.weight).abs();
            let zone = listener_sample
                .zone
                .expect("a non-exterior same-zone transition has authority");
            portal_extension_m = f64::from(zone.transition_depth_m + pose_uncertainty_m);
            accumulate_scaled(
                &mut boundary_gain_db,
                zone.boundary_gain_db,
                crossing_weight,
            );
            if listener_sample.weight < source_sample.weight {
                path_listener_id = EXTERIOR_ZONE_ID;
            } else {
                path_source_id = EXTERIOR_ZONE_ID;
            }
        } else {
            if let Some(zone) = listener_sample.zone {
                accumulate_scaled(
                    &mut boundary_gain_db,
                    zone.boundary_gain_db,
                    listener_sample.weight,
                );
            }
            if let Some(zone) = source_sample.zone {
                accumulate_scaled(
                    &mut boundary_gain_db,
                    zone.boundary_gain_db,
                    source_sample.weight,
                );
            }
            crossing_weight = listener_sample.weight.max(source_sample.weight);
        }
        let mut selected_portal = None;
        let connected = self
            .portals_by_zone_pair
            .get(&zone_pair_key(path_listener_id, path_source_id))
            .map_or([].as_slice(), Vec::as_slice);
        for portal_index in connected {
            let portal = &self.portals[*portal_index];
            let Some(aperture_weight) =
                portal_intersection_weight(portal, source, listener, portal_extension_m)
            else {
                continue;
            };
            let portal_gain = match portal.state {
                StaticPortalState::Open => portal.open_gain_db,
                StaticPortalState::Closed => portal.closed_gain_db,
            };
            // Fused blend and score accumulation. Per-band arithmetic and the
            // left-to-right summation order match the former scale_curve ->
            // blend_curves -> sum pipeline exactly, so portal-selection ties
            // still resolve on bit-identical scores.
            let mut candidate = [0.0_f32; SPECTRAL_BAND_COUNT];
            let mut score = 0.0_f64;
            for band in 0..SPECTRAL_BAND_COUNT {
                let scaled = portal_gain[band] * crossing_weight;
                let blended =
                    boundary_gain_db[band] + (scaled - boundary_gain_db[band]) * aperture_weight;
                candidate[band] = blended;
                score += f64::from(blended);
            }
            if selected_portal
                .as_ref()
                .is_none_or(|selected: &SelectedPortal| score > selected.score)
            {
                selected_portal = Some(SelectedPortal {
                    id: portal.id,
                    state: portal.state,
                    gain_db: candidate,
                    score,
                });
            }
        }

        let (enclosure_gain_db, authority) = selected_portal.map_or(
            (boundary_gain_db, EnclosureAuthority::ZoneBoundary),
            |portal| {
                (
                    portal.gain_db,
                    match portal.state {
                        StaticPortalState::Open => EnclosureAuthority::OpenPortal(portal.id),
                        StaticPortalState::Closed => EnclosureAuthority::ClosedPortal(portal.id),
                    },
                )
            },
        );
        Ok(EnclosureEvaluation {
            listener_zone_id: listener_sample.id,
            source_zone_id: source_sample.id,
            listener_zone_weight: listener_sample.weight,
            enclosure_gain_db,
            diffuse_field,
            authority,
        })
    }

    fn sample_zone(&self, position: EnuVector3, pose_uncertainty_m: f32) -> ZoneSample<'_> {
        let mut selected: Option<ZoneSample<'_>> = None;
        for zone in &self.zones {
            let Some(inward_depth_m) = inward_depth(zone, position) else {
                continue;
            };
            let transition_m = f64::from(zone.transition_depth_m + pose_uncertainty_m);
            let weight = if transition_m <= f64::EPSILON {
                1.0
            } else {
                smoothstep((inward_depth_m / transition_m).clamp(0.0, 1.0)) as f32
            };
            let candidate = ZoneSample {
                id: zone.id,
                weight,
                priority: zone.priority,
                zone: Some(zone),
            };
            if selected.as_ref().is_none_or(|current| {
                zone.priority > current.priority
                    || (zone.priority == current.priority
                        && (weight > current.weight
                            || (weight == current.weight && zone.id < current.id)))
            }) {
                selected = Some(candidate);
            }
        }
        selected.unwrap_or(ZoneSample {
            id: EXTERIOR_ZONE_ID,
            weight: 0.0,
            priority: 0,
            zone: None,
        })
    }
}

#[derive(Clone, Copy)]
struct ZoneSample<'a> {
    id: AcousticZoneId,
    weight: f32,
    /// Zone priority hoisted out of the selection comparator's unwrap chain.
    priority: u16,
    zone: Option<&'a AcousticZone>,
}

struct SelectedPortal {
    id: StaticPortalId,
    state: StaticPortalState,
    gain_db: [f32; SPECTRAL_BAND_COUNT],
    score: f64,
}

fn inward_depth(zone: &AcousticZone, point: EnuVector3) -> Option<f64> {
    let distances = [
        f64::from(point.east_m - zone.bounds.minimum.east_m),
        f64::from(zone.bounds.maximum.east_m - point.east_m),
        f64::from(point.north_m - zone.bounds.minimum.north_m),
        f64::from(zone.bounds.maximum.north_m - point.north_m),
        f64::from(point.up_m - zone.bounds.minimum.up_m),
        f64::from(zone.bounds.maximum.up_m - point.up_m),
    ];
    distances
        .into_iter()
        .reduce(f64::min)
        .filter(|depth| *depth >= 0.0)
}

/// Unordered zone-pair key; both traversal directions share one portal list.
fn zone_pair_key(left: AcousticZoneId, right: AcousticZoneId) -> (AcousticZoneId, AcousticZoneId) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

fn portal_intersection_weight(
    portal: &StaticPortal,
    source: EnuVector3,
    listener: EnuVector3,
    endpoint_extension_m: f64,
) -> Option<f32> {
    let source = vector(source);
    let listener = vector(listener);
    let center = vector(portal.center);
    let normal = vector(portal.normal);
    let up = vector(portal.up);
    let right = cross(normal, up);
    let direction = subtract(listener, source);
    let denominator = dot(direction, normal);
    if denominator.abs() <= 1.0e-9 {
        return None;
    }
    let t = dot(subtract(center, source), normal) / denominator;
    let hit = add(source, scale(direction, t));
    if !(0.0..=1.0).contains(&t) {
        let endpoint = if t < 0.0 { source } else { listener };
        let offset = subtract(hit, endpoint);
        let extension_distance_m = dot(offset, offset).sqrt();
        if endpoint_extension_m <= 0.0 || extension_distance_m > endpoint_extension_m {
            return None;
        }
    }
    let offset = subtract(hit, center);
    let horizontal = dot(offset, right).abs();
    let vertical = dot(offset, up).abs();
    let horizontal_margin = f64::from(portal.half_width_m) - horizontal;
    let vertical_margin = f64::from(portal.half_height_m) - vertical;
    let margin = horizontal_margin.min(vertical_margin);
    if margin < 0.0 {
        return None;
    }
    if portal.edge_transition_m <= f32::EPSILON {
        Some(1.0)
    } else {
        Some(smoothstep((margin / f64::from(portal.edge_transition_m)).clamp(0.0, 1.0)) as f32)
    }
}

fn accumulate_scaled(
    output: &mut [f32; SPECTRAL_BAND_COUNT],
    curve: [f32; SPECTRAL_BAND_COUNT],
    weight: f32,
) {
    for index in 0..SPECTRAL_BAND_COUNT {
        output[index] += curve[index] * weight;
    }
}

fn scale_diffuse(profile: DiffuseFieldProfile, weight: f32) -> DiffuseFieldProfile {
    let off = DiffuseFieldProfile::OFF;
    DiffuseFieldProfile {
        wet_gain: profile.wet_gain * weight,
        rt60_s: off.rt60_s + (profile.rt60_s - off.rt60_s) * weight,
        high_frequency_damping: off.high_frequency_damping
            + (profile.high_frequency_damping - off.high_frequency_damping) * weight,
    }
}

fn smoothstep(value: f64) -> f64 {
    value * value * (3.0 - 2.0 * value)
}

type Vector = [f64; 3];

fn vector(value: EnuVector3) -> Vector {
    [
        f64::from(value.east_m),
        f64::from(value.north_m),
        f64::from(value.up_m),
    ]
}

fn add(left: Vector, right: Vector) -> Vector {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

fn subtract(left: Vector, right: Vector) -> Vector {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn scale(vector: Vector, factor: f64) -> Vector {
    [vector[0] * factor, vector[1] * factor, vector[2] * factor]
}

fn dot(left: Vector, right: Vector) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn cross(left: Vector, right: Vector) -> Vector {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnclosureEvaluationError {
    NonFinitePosition,
    InvalidPoseUncertainty,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_api::enclosure::{AcousticZoneKind, AxisAlignedZoneBounds, EnclosureProvenance};

    const HOME: AcousticZoneId = AcousticZoneId(1);
    const DOOR: StaticPortalId = StaticPortalId(1);

    fn home() -> AcousticZone {
        AcousticZone {
            id: HOME,
            kind: AcousticZoneKind::Interior,
            bounds: AxisAlignedZoneBounds {
                minimum: EnuVector3::new(0.0, 0.0, 0.0),
                maximum: EnuVector3::new(10.0, 10.0, 3.0),
            },
            priority: 1,
            transition_depth_m: 1.0,
            boundary_gain_db: [-8.0, -9.0, -11.0, -14.0, -18.0, -23.0, -28.0, -32.0],
            diffuse_field: DiffuseFieldProfile::SMALL_INTERIOR,
            provenance: EnclosureProvenance::AuthoredStatic,
        }
    }

    fn door(state: StaticPortalState) -> StaticPortal {
        StaticPortal {
            id: DOOR,
            zone_a: EXTERIOR_ZONE_ID,
            zone_b: HOME,
            center: EnuVector3::new(0.0, 5.0, 1.2),
            normal: EnuVector3::new(1.0, 0.0, 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
            half_width_m: 0.55,
            half_height_m: 1.2,
            edge_transition_m: 0.1,
            state,
            open_gain_db: [-0.5, -0.5, -0.7, -1.0, -1.3, -1.8, -2.5, -3.0],
            closed_gain_db: [-6.0, -7.0, -9.0, -12.0, -16.0, -21.0, -27.0, -32.0],
            provenance: EnclosureProvenance::AuthoredStatic,
        }
    }

    #[test]
    fn owner_home_open_door_is_audibly_clearer_than_the_wall_or_closed_door() {
        let listener = EnuVector3::new(5.0, 5.0, 1.5);
        let source = EnuVector3::new(-5.0, 5.0, 1.5);
        let wall = EnclosureScene::new(vec![home()], vec![])
            .unwrap()
            .evaluate(listener, source, 0.0)
            .unwrap();
        let open = EnclosureScene::new(vec![home()], vec![door(StaticPortalState::Open)])
            .unwrap()
            .evaluate(listener, source, 0.0)
            .unwrap();
        let closed = EnclosureScene::new(vec![home()], vec![door(StaticPortalState::Closed)])
            .unwrap()
            .evaluate(listener, source, 0.0)
            .unwrap();

        assert_eq!(open.authority, EnclosureAuthority::OpenPortal(DOOR));
        assert_eq!(closed.authority, EnclosureAuthority::ClosedPortal(DOOR));
        assert!(open.enclosure_gain_db[5] > closed.enclosure_gain_db[5]);
        assert!(closed.enclosure_gain_db[5] > wall.enclosure_gain_db[5]);
        assert_eq!(open.diffuse_field, DiffuseFieldProfile::SMALL_INTERIOR);
        println!(
            "ENCLOSURE_SMOKE wall_4khz_db={:.1} closed_4khz_db={:.1} open_4khz_db={:.1} wet_gain={:.2}",
            wall.enclosure_gain_db[5],
            closed.enclosure_gain_db[5],
            open.enclosure_gain_db[5],
            open.diffuse_field.wet_gain
        );
    }

    #[test]
    fn same_room_stays_spectrally_neutral_but_keeps_the_shared_tail() {
        let scene = EnclosureScene::new(vec![home()], vec![]).unwrap();
        let evaluation = scene
            .evaluate(
                EnuVector3::new(5.0, 5.0, 1.5),
                EnuVector3::new(7.0, 5.0, 1.5),
                0.0,
            )
            .unwrap();
        assert_eq!(evaluation.authority, EnclosureAuthority::SameZone);
        assert_eq!(evaluation.enclosure_gain_db, [0.0; SPECTRAL_BAND_COUNT]);
        assert_eq!(
            evaluation.diffuse_field,
            DiffuseFieldProfile::SMALL_INTERIOR
        );
    }

    #[test]
    fn boundary_depth_and_pose_uncertainty_smooth_the_listener_profile() {
        let scene = EnclosureScene::new(vec![home()], vec![]).unwrap();
        let near = scene
            .evaluate(
                EnuVector3::new(0.1, 5.0, 1.5),
                EnuVector3::new(-5.0, 5.0, 1.5),
                0.4,
            )
            .unwrap();
        let deep = scene
            .evaluate(
                EnuVector3::new(5.0, 5.0, 1.5),
                EnuVector3::new(-5.0, 5.0, 1.5),
                0.4,
            )
            .unwrap();
        assert!(near.listener_zone_weight < deep.listener_zone_weight);
        assert!(near.diffuse_field.wet_gain < deep.diffuse_field.wet_gain);
        assert!(near.enclosure_gain_db[5] > deep.enclosure_gain_db[5]);
    }

    #[test]
    fn doorway_transition_is_continuous_across_the_exterior_plane() {
        let scene = EnclosureScene::new(vec![home()], vec![door(StaticPortalState::Open)]).unwrap();
        let source = EnuVector3::new(5.0, 5.0, 1.5);
        let outside = scene
            .evaluate(EnuVector3::new(-0.001, 5.0, 1.5), source, 0.0)
            .unwrap();
        let plane = scene
            .evaluate(EnuVector3::new(0.0, 5.0, 1.5), source, 0.0)
            .unwrap();
        let inside = scene
            .evaluate(EnuVector3::new(0.001, 5.0, 1.5), source, 0.0)
            .unwrap();

        assert_eq!(outside.authority, EnclosureAuthority::OpenPortal(DOOR));
        assert_eq!(plane.authority, EnclosureAuthority::OpenPortal(DOOR));
        assert_eq!(inside.authority, EnclosureAuthority::OpenPortal(DOOR));
        for band in 0..SPECTRAL_BAND_COUNT {
            assert!(
                (outside.enclosure_gain_db[band] - plane.enclosure_gain_db[band]).abs() < 0.001
            );
            assert!((plane.enclosure_gain_db[band] - inside.enclosure_gain_db[band]).abs() < 0.001);
        }
    }

    #[test]
    fn canonical_doorway_motion_changes_no_band_more_than_quarter_db_per_block() {
        const SAMPLE_RATE_HZ: f32 = 48_000.0;
        const BLOCK_FRAMES: f32 = 128.0;
        const LISTENER_SPEED_MPS: f32 = 1.4;
        let step_m = LISTENER_SPEED_MPS * BLOCK_FRAMES / SAMPLE_RATE_HZ;
        let scene =
            EnclosureScene::new(vec![home()], vec![door(StaticPortalState::Closed)]).unwrap();
        let source = EnuVector3::new(5.0, 5.0, 1.5);
        let mut x = -0.05_f32;
        let mut previous = scene
            .evaluate(EnuVector3::new(x, 5.0, 1.5), source, 0.0)
            .unwrap();
        let mut maximum_step_db = 0.0_f32;
        while x < 1.05 {
            x += step_m;
            let current = scene
                .evaluate(EnuVector3::new(x, 5.0, 1.5), source, 0.0)
                .unwrap();
            for band in 0..SPECTRAL_BAND_COUNT {
                maximum_step_db = maximum_step_db.max(
                    (current.enclosure_gain_db[band] - previous.enclosure_gain_db[band]).abs(),
                );
            }
            previous = current;
        }
        println!("GAMMA7_SLEW maximum_adjacent_step_db={maximum_step_db:.6}");
        assert!(
            maximum_step_db <= 0.25,
            "canonical crossing changed {maximum_step_db:.6} dB in one 128-frame block"
        );
    }

    #[test]
    fn evaluation_publishes_only_the_named_enclosure_stage() {
        let scene = EnclosureScene::new(vec![home()], vec![]).unwrap();
        let evaluation = scene
            .evaluate(
                EnuVector3::new(5.0, 5.0, 1.5),
                EnuVector3::new(-5.0, 5.0, 1.5),
                0.0,
            )
            .unwrap();
        let mut transfer = SpectralTransfer::NEUTRAL;
        evaluation.publish(&mut transfer).unwrap();
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Enclosure),
            evaluation.enclosure_gain_db
        );
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Atmosphere),
            [0.0; SPECTRAL_BAND_COUNT]
        );
    }

    #[test]
    fn gamma7_has_four_named_owner_home_states_with_one_composed_enclosure_stage() {
        // These are the four γ7 card states. “Doorway” is a crossing pose, not
        // a second physical source or an implicit delayed portal renderer.
        let scene = EnclosureScene::new(vec![home()], vec![door(StaticPortalState::Open)]).unwrap();
        let exterior = scene
            .evaluate(
                EnuVector3::new(-5.0, 5.0, 1.5),
                EnuVector3::new(-10.0, 5.0, 1.5),
                0.0,
            )
            .unwrap();
        let closed_facade = EnclosureScene::new(vec![home()], vec![])
            .unwrap()
            .evaluate(
                EnuVector3::new(5.0, 5.0, 1.5),
                EnuVector3::new(-5.0, 5.0, 1.5),
                0.0,
            )
            .unwrap();
        let open_window = scene
            .evaluate(
                EnuVector3::new(5.0, 5.0, 1.5),
                EnuVector3::new(-5.0, 5.0, 1.5),
                0.0,
            )
            .unwrap();
        let doorway = scene
            .evaluate(
                EnuVector3::new(0.001, 5.0, 1.5),
                EnuVector3::new(5.0, 5.0, 1.5),
                0.0,
            )
            .unwrap();

        assert_eq!(exterior.authority, EnclosureAuthority::Exterior);
        assert_eq!(closed_facade.authority, EnclosureAuthority::ZoneBoundary);
        assert_eq!(open_window.authority, EnclosureAuthority::OpenPortal(DOOR));
        assert_eq!(doorway.authority, EnclosureAuthority::OpenPortal(DOOR));
        assert_eq!(exterior.enclosure_gain_db, [0.0; SPECTRAL_BAND_COUNT]);
        assert_eq!(exterior.diffuse_field, DiffuseFieldProfile::OFF);
        assert_eq!(
            closed_facade.diffuse_field,
            DiffuseFieldProfile::SMALL_INTERIOR
        );
        assert_eq!(
            open_window.diffuse_field,
            DiffuseFieldProfile::SMALL_INTERIOR
        );
        assert!(doorway.diffuse_field.wet_gain > 0.0);
        assert!(doorway.diffuse_field.wet_gain < DiffuseFieldProfile::SMALL_INTERIOR.wet_gain);
        assert!(open_window.enclosure_gain_db[5] > closed_facade.enclosure_gain_db[5]);

        for (name, evaluation) in [
            ("exterior", exterior),
            ("closed_facade", closed_facade),
            ("open_window", open_window),
            ("doorway", doorway),
        ] {
            let mut transfer = SpectralTransfer::NEUTRAL;
            evaluation.publish(&mut transfer).unwrap();
            for stage in SpectralStage::ALL {
                if stage == SpectralStage::Enclosure {
                    assert_eq!(
                        transfer.stage_gain_db(stage),
                        evaluation.enclosure_gain_db,
                        "{name} enclosure stage"
                    );
                } else {
                    assert_eq!(
                        transfer.stage_gain_db(stage),
                        [0.0; SPECTRAL_BAND_COUNT],
                        "{name} unexpectedly populated {stage:?}"
                    );
                }
            }
            assert!(transfer.combined_gain_db().into_iter().all(f32::is_finite));
            println!(
                "GAMMA7_STATE name={name} authority={:?} gain_4khz_db={:.4} wet_gain={:.8}",
                evaluation.authority,
                evaluation.enclosure_gain_db[5],
                evaluation.diffuse_field.wet_gain
            );
        }
    }

    #[test]
    fn gamma7_has_immediate_direct_filter_and_separate_diffuse_tail_without_338hz_comb() {
        use crate::{SharedDiffuseField, SpectralTransferFilter};

        const SAMPLE_RATE_HZ: u32 = 48_000;
        const BLOCK_FRAMES: usize = 128;
        let scene = EnclosureScene::new(vec![home()], vec![door(StaticPortalState::Open)]).unwrap();
        let evaluation = scene
            .evaluate(
                EnuVector3::new(5.0, 5.0, 1.5),
                EnuVector3::new(-5.0, 5.0, 1.5),
                0.0,
            )
            .unwrap();
        let mut transfer = SpectralTransfer::NEUTRAL;
        evaluation.publish(&mut transfer).unwrap();

        // The portal contribution is a gain-only contribution to the one
        // composed filter: an impulse has an immediate direct sample. Any
        // delayed energy is supplied by the separately instantiated shared
        // diffuse field below, never by the portal evaluation.
        let mut filter = SpectralTransferFilter::new(SAMPLE_RATE_HZ).unwrap();
        filter.set_transfer(transfer);
        let direct_first = filter.process_sample(1.0);
        assert!(direct_first.is_finite() && direct_first.abs() > 1.0e-3);

        let mut diffuse =
            SharedDiffuseField::new(SAMPLE_RATE_HZ, DiffuseFieldProfile::SMALL_INTERIOR).unwrap();
        let mut diffuse_first_frame = None;
        for block in 0..400 {
            let mut input = [0.0_f32; BLOCK_FRAMES];
            if block == 0 {
                input[0] = 1.0;
            }
            let mut left = [0.0_f32; BLOCK_FRAMES];
            let mut right = [0.0_f32; BLOCK_FRAMES];
            diffuse
                .process_block(&input, &mut left, &mut right)
                .unwrap();
            for frame in 0..BLOCK_FRAMES {
                if diffuse_first_frame.is_none() && (left[frame] != 0.0 || right[frame] != 0.0) {
                    diffuse_first_frame = Some(block * BLOCK_FRAMES + frame);
                }
            }
        }
        let diffuse_first_frame = diffuse_first_frame.expect("shared diffuse tail must emerge");
        assert!(diffuse_first_frame > 0);

        fn steady_sine_db(
            transfer: SpectralTransfer,
            frequency_hz: f32,
            sample_rate_hz: u32,
        ) -> f32 {
            const FRAMES: usize = 96_000;
            const MEASURE_FROM: usize = 48_000;
            let mut filter = SpectralTransferFilter::new(sample_rate_hz).unwrap();
            filter.set_transfer(transfer);
            let mut energy = 0.0_f64;
            for frame in 0..FRAMES {
                let phase =
                    core::f32::consts::TAU * frequency_hz * frame as f32 / sample_rate_hz as f32;
                let output = filter.process_sample(phase.sin());
                if frame >= MEASURE_FROM {
                    energy += f64::from(output * output);
                }
            }
            let rms = (energy / (FRAMES - MEASURE_FROM) as f64).sqrt();
            (20.0 * (rms as f32).log10()) - 20.0 * (0.5_f32).sqrt().log10()
        }

        // A doorway comb would present as a narrow notch near the rejected
        // donor's 338 Hz excess-delay signature. Compare that bin to smooth
        // neighbors in the isolated direct/filter stem. This is a numerical
        // anti-comb check, not a perceptual or device/listening claim.
        let at_320_hz = steady_sine_db(transfer, 320.0, SAMPLE_RATE_HZ);
        let at_338_hz = steady_sine_db(transfer, 338.0, SAMPLE_RATE_HZ);
        let at_360_hz = steady_sine_db(transfer, 360.0, SAMPLE_RATE_HZ);
        let interpolated_338_hz = (at_320_hz + at_360_hz) * 0.5;
        let notch_residual_db = (at_338_hz - interpolated_338_hz).abs();
        assert!(
            notch_residual_db <= 0.25,
            "338 Hz neighborhood has a {notch_residual_db:.4} dB notch residual"
        );
        assert!((at_320_hz - at_360_hz).abs() <= 0.5);
        println!(
            "GAMMA7_MECHANICAL direct_first={direct_first:.7} diffuse_first_frame={diffuse_first_frame} diffuse_first_ms={:.3} response_320_db={at_320_hz:.4} response_338_db={at_338_hz:.4} response_360_db={at_360_hz:.4} notch_residual_db={notch_residual_db:.4}",
            diffuse_first_frame as f32 * 1000.0 / SAMPLE_RATE_HZ as f32
        );
    }
}
