//! Source-orientation evaluation and composed spectral publication for V1
//! axisymmetric directivity.

use fightbox_api::directivity::{AxisymmetricDirectivityAngleError, AxisymmetricDirectivityTable};
use fightbox_api::spectral::{
    SPECTRAL_BAND_COUNT, SpectralStage, SpectralTransfer, SpectralTransferError,
};
use fightbox_api::{Directivity, EnuVector3, Pose};

/// One orientation sample from an attached V1 polar table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisymmetricDirectivityEvaluation {
    polar_angle_degrees: f32,
    direct_gain_db: [f32; SPECTRAL_BAND_COUNT],
    sphere_averaged_indirect_power_gain: f32,
}

impl AxisymmetricDirectivityEvaluation {
    #[must_use]
    pub const fn polar_angle_degrees(self) -> f32 {
        self.polar_angle_degrees
    }

    #[must_use]
    pub const fn direct_gain_db(self) -> [f32; SPECTRAL_BAND_COUNT] {
        self.direct_gain_db
    }

    /// Listener-independent power scalar for the indirect source send.
    #[must_use]
    pub const fn sphere_averaged_indirect_power_gain(self) -> f32 {
        self.sphere_averaged_indirect_power_gain
    }
}

/// Which source-radiation path a publication selected.
///
/// An absent V1 table keeps the existing Steam-compatible broadband dipole
/// descriptor byte-for-byte. An attached table owns direct coloration through
/// `SpectralTransfer`, so [`Self::broadband_directivity_for_backend`] returns
/// the neutral dipole and prevents applying both models.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DirectivityPublication {
    LegacyDipole(Directivity),
    Axisymmetric(AxisymmetricDirectivityEvaluation),
}

impl DirectivityPublication {
    /// Broadband descriptor the propagation backend should apply.
    #[must_use]
    pub const fn broadband_directivity_for_backend(self) -> Directivity {
        match self {
            Self::LegacyDipole(directivity) => directivity,
            Self::Axisymmetric(_) => Directivity::OMNIDIRECTIONAL,
        }
    }

    /// Table-derived indirect power, or `None` while the legacy path owns it.
    #[must_use]
    pub const fn sphere_averaged_indirect_power_gain(self) -> Option<f32> {
        match self {
            Self::LegacyDipole(_) => None,
            Self::Axisymmetric(evaluation) => {
                Some(evaluation.sphere_averaged_indirect_power_gain())
            }
        }
    }
}

/// Evaluates the polar angle from source forward to source-to-listener.
pub fn evaluate_axisymmetric_directivity(
    table: &AxisymmetricDirectivityTable,
    source_pose: Pose,
    listener_position: EnuVector3,
) -> Result<AxisymmetricDirectivityEvaluation, DirectivityTransferError> {
    if !source_pose.position.is_finite() {
        return Err(DirectivityTransferError::NonFiniteSourcePosition);
    }
    if !source_pose.forward.is_finite() {
        return Err(DirectivityTransferError::NonFiniteSourceForward);
    }
    if !listener_position.is_finite() {
        return Err(DirectivityTransferError::NonFiniteListenerPosition);
    }

    let forward = [
        f64::from(source_pose.forward.east_m),
        f64::from(source_pose.forward.north_m),
        f64::from(source_pose.forward.up_m),
    ];
    let source_to_listener = [
        f64::from(listener_position.east_m) - f64::from(source_pose.position.east_m),
        f64::from(listener_position.north_m) - f64::from(source_pose.position.north_m),
        f64::from(listener_position.up_m) - f64::from(source_pose.position.up_m),
    ];
    let forward_length = vector_length(forward);
    if forward_length == 0.0 {
        return Err(DirectivityTransferError::DegenerateSourceForward);
    }
    let listener_distance = vector_length(source_to_listener);
    if listener_distance == 0.0 {
        return Err(DirectivityTransferError::CoincidentSourceAndListener);
    }

    let dot = (forward[0] * source_to_listener[0]
        + forward[1] * source_to_listener[1]
        + forward[2] * source_to_listener[2])
        / (forward_length * listener_distance);
    let bounded_dot = dot.clamp(-1.0, 1.0);
    let polar_angle_degrees = if bounded_dot == 1.0 {
        0.0
    } else if bounded_dot == -1.0 {
        180.0
    } else {
        bounded_dot.acos().to_degrees() as f32
    };
    let direct_gain_db = table
        .gain_db_at_polar_angle(polar_angle_degrees)
        .map_err(DirectivityTransferError::TableAngle)?;

    Ok(AxisymmetricDirectivityEvaluation {
        polar_angle_degrees,
        direct_gain_db,
        sphere_averaged_indirect_power_gain: table.sphere_averaged_indirect_power_gain(),
    })
}

/// Publishes either an attached polar table or the unchanged legacy path.
///
/// The optional table is the attachment boundary. `None` clears only the named
/// spectral Directivity stem and returns the caller's exact legacy descriptor.
/// `Some` evaluates orientation, publishes its eight bands, and recommends a
/// neutral backend dipole through [`DirectivityPublication`].
pub fn publish_source_directivity(
    legacy_directivity: Directivity,
    table: Option<&AxisymmetricDirectivityTable>,
    source_pose: Pose,
    listener_position: EnuVector3,
    transfer: &mut SpectralTransfer,
) -> Result<DirectivityPublication, DirectivityTransferError> {
    let Some(table) = table else {
        transfer.clear_stage(SpectralStage::Directivity);
        return Ok(DirectivityPublication::LegacyDipole(legacy_directivity));
    };

    let evaluation = evaluate_axisymmetric_directivity(table, source_pose, listener_position)?;
    transfer
        .set_stage(SpectralStage::Directivity, evaluation.direct_gain_db())
        .map_err(DirectivityTransferError::SpectralTransfer)?;
    Ok(DirectivityPublication::Axisymmetric(evaluation))
}

/// Rejected orientation or composed-transfer publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectivityTransferError {
    NonFiniteSourcePosition,
    NonFiniteSourceForward,
    DegenerateSourceForward,
    NonFiniteListenerPosition,
    CoincidentSourceAndListener,
    TableAngle(AxisymmetricDirectivityAngleError),
    SpectralTransfer(SpectralTransferError),
}

fn vector_length(vector: [f64; 3]) -> f64 {
    (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpectralTransferFilter;
    use fightbox_api::directivity::AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT;

    fn source_pose(forward: EnuVector3) -> Pose {
        Pose {
            position: EnuVector3::new(0.0, 0.0, 0.0),
            forward,
            up: EnuVector3::new(0.0, 0.0, 1.0),
        }
    }

    fn test_table() -> AxisymmetricDirectivityTable {
        let rear_loss_db = [-3.0, -4.0, -6.0, -9.0, -12.0, -18.0, -24.0, -30.0];
        AxisymmetricDirectivityTable::new(core::array::from_fn(|band_index| {
            core::array::from_fn(|angle_index| {
                rear_loss_db[band_index] * angle_index as f32
                    / (AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT - 1) as f32
            })
        }))
        .unwrap()
    }

    #[test]
    fn source_orientation_hits_front_side_back_nodes_and_is_axisymmetric() {
        let table = test_table();
        let pose = source_pose(EnuVector3::new(0.0, 2.0, 0.0));
        let front =
            evaluate_axisymmetric_directivity(&table, pose, EnuVector3::new(0.0, 10.0, 0.0))
                .unwrap();
        let side_east =
            evaluate_axisymmetric_directivity(&table, pose, EnuVector3::new(10.0, 0.0, 0.0))
                .unwrap();
        let side_up =
            evaluate_axisymmetric_directivity(&table, pose, EnuVector3::new(0.0, 0.0, 10.0))
                .unwrap();
        let back =
            evaluate_axisymmetric_directivity(&table, pose, EnuVector3::new(0.0, -10.0, 0.0))
                .unwrap();

        assert_eq!(front.polar_angle_degrees(), 0.0);
        assert_eq!(front.direct_gain_db(), [0.0; 8]);
        assert_eq!(side_east.polar_angle_degrees(), 90.0);
        assert_eq!(side_east.direct_gain_db(), side_up.direct_gain_db());
        assert_eq!(
            side_east.direct_gain_db(),
            [-1.5, -2.0, -3.0, -4.5, -6.0, -9.0, -12.0, -15.0]
        );
        assert_eq!(back.polar_angle_degrees(), 180.0);
        assert_eq!(
            back.direct_gain_db(),
            [-3.0, -4.0, -6.0, -9.0, -12.0, -18.0, -24.0, -30.0]
        );
        assert_eq!(
            front.sphere_averaged_indirect_power_gain().to_bits(),
            back.sphere_averaged_indirect_power_gain().to_bits()
        );
    }

    #[test]
    fn absent_table_clears_only_directivity_and_preserves_exact_legacy_dipole() {
        let legacy = Directivity {
            dipole_weight: 0.73,
            dipole_power: 2.25,
        };
        let mut transfer = SpectralTransfer::NEUTRAL;
        transfer
            .set_stage(SpectralStage::Directivity, [-9.0; 8])
            .unwrap();
        transfer
            .set_stage(SpectralStage::Ground, [-2.0; 8])
            .unwrap();
        let invalid_unused_pose = source_pose(EnuVector3::new(f32::NAN, 0.0, 0.0));

        let publication = publish_source_directivity(
            legacy,
            None,
            invalid_unused_pose,
            EnuVector3::new(f32::NAN, 0.0, 0.0),
            &mut transfer,
        )
        .unwrap();

        assert_eq!(publication, DirectivityPublication::LegacyDipole(legacy));
        assert_eq!(publication.broadband_directivity_for_backend(), legacy);
        assert_eq!(publication.sphere_averaged_indirect_power_gain(), None);
        assert_eq!(transfer.stage_gain_db(SpectralStage::Directivity), [0.0; 8]);
        assert_eq!(transfer.stage_gain_db(SpectralStage::Ground), [-2.0; 8]);
    }

    #[test]
    fn attached_table_replaces_broadband_dipole_and_error_is_non_mutating() {
        let table = test_table();
        let legacy = Directivity {
            dipole_weight: 0.8,
            dipole_power: 4.0,
        };
        let mut transfer = SpectralTransfer::NEUTRAL;
        transfer
            .set_stage(SpectralStage::Atmosphere, [-1.0; 8])
            .unwrap();
        let publication = publish_source_directivity(
            legacy,
            Some(&table),
            source_pose(EnuVector3::new(0.0, 1.0, 0.0)),
            EnuVector3::new(0.0, -10.0, 0.0),
            &mut transfer,
        )
        .unwrap();

        assert_eq!(
            publication.broadband_directivity_for_backend(),
            Directivity::OMNIDIRECTIONAL
        );
        assert!(
            publication
                .sphere_averaged_indirect_power_gain()
                .is_some_and(|gain| gain > 0.0 && gain < 1.0)
        );
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Directivity),
            [-3.0, -4.0, -6.0, -9.0, -12.0, -18.0, -24.0, -30.0]
        );
        assert_eq!(transfer.stage_gain_db(SpectralStage::Atmosphere), [-1.0; 8]);

        let before_error = transfer;
        assert_eq!(
            publish_source_directivity(
                legacy,
                Some(&table),
                source_pose(EnuVector3::new(0.0, 0.0, 0.0)),
                EnuVector3::new(0.0, -10.0, 0.0),
                &mut transfer,
            ),
            Err(DirectivityTransferError::DegenerateSourceForward)
        );
        assert_eq!(transfer, before_error);
    }

    fn filtered_tone_rms(transfer: SpectralTransfer, frequency_hz: f32) -> f64 {
        const SAMPLE_RATE: u32 = 48_000;
        const FRAMES: usize = SAMPLE_RATE as usize;
        let mut filter = SpectralTransferFilter::new(SAMPLE_RATE).unwrap();
        filter.set_transfer(transfer);
        let mut energy = 0.0_f64;
        for frame in 0..FRAMES {
            let input =
                (core::f32::consts::TAU * frequency_hz * frame as f32 / SAMPLE_RATE as f32).sin();
            let output = filter.process_sample(input);
            if frame >= FRAMES / 2 {
                energy += f64::from(output * output);
            }
        }
        (energy / (FRAMES / 2) as f64).sqrt()
    }

    #[test]
    fn front_to_back_turn_keeps_body_and_dulls_the_upper_spectrum() {
        let table = test_table();
        let pose = source_pose(EnuVector3::new(0.0, 1.0, 0.0));
        let mut front = SpectralTransfer::NEUTRAL;
        let mut back = SpectralTransfer::NEUTRAL;
        let back_publication = publish_source_directivity(
            Directivity::OMNIDIRECTIONAL,
            Some(&table),
            pose,
            EnuVector3::new(0.0, 10.0, 0.0),
            &mut front,
        )
        .unwrap();
        publish_source_directivity(
            Directivity::OMNIDIRECTIONAL,
            Some(&table),
            pose,
            EnuVector3::new(0.0, -10.0, 0.0),
            &mut back,
        )
        .unwrap();

        assert!(front.is_neutral());
        let front_body = filtered_tone_rms(front, 125.0);
        let front_presence = filtered_tone_rms(front, 4_000.0);
        let back_body = filtered_tone_rms(back, 125.0);
        let back_presence = filtered_tone_rms(back, 4_000.0);
        let indirect_power = back_publication
            .sphere_averaged_indirect_power_gain()
            .unwrap();
        println!(
            "DIRECTIVITY_TURN front_125_rms={front_body:.6} back_125_rms={back_body:.6} front_4000_rms={front_presence:.6} back_4000_rms={back_presence:.6} indirect_power={indirect_power:.6}"
        );

        assert!(indirect_power > 0.0 && indirect_power < 1.0);
        assert!(back_body > 0.40);
        assert!(back_body < front_body);
        assert!(back_presence < front_presence * 0.45);
        assert!(back_presence < back_body * 0.55);
    }
}
