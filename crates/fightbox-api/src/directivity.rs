//! Axisymmetric eight-band source-radiation tables.
//!
//! V1 samples polar angle from the source's forward axis at fixed ten-degree
//! nodes. Azimuth is deliberately absent: a source rotated around its forward
//! axis has the same response. Values are amplitude changes in dB relative to
//! the source's declared on-axis level at one metre.

use crate::spectral::SPECTRAL_BAND_COUNT;

/// Number of fixed polar-angle nodes in one V1 directivity band.
pub const AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT: usize = 19;

/// Separation between adjacent V1 polar-angle nodes.
pub const AXISYMMETRIC_DIRECTIVITY_ANGLE_STEP_DEGREES: f32 = 10.0;

/// Fixed polar angles from the forward axis through the rear axis.
pub const AXISYMMETRIC_DIRECTIVITY_ANGLES_DEGREES: [f32; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT] = [
    0.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 110.0, 120.0, 130.0, 140.0,
    150.0, 160.0, 170.0, 180.0,
];

/// Validated 8-band by 19-angle V1 polar response.
///
/// The first index follows `SPECTRAL_BAND_CENTERS_HZ`; the second follows
/// [`AXISYMMETRIC_DIRECTIVITY_ANGLES_DEGREES`]. Each band's on-axis node is
/// exactly `0 dB`, and every off-axis node is non-positive. Linear
/// interpolation occurs in dB, preserving the authored node values exactly.
///
/// The cached indirect-power scalar assumes equal source power in each fixed
/// octave band because the current source contract has no per-band source-power
/// weights. Its angular average is taken in linear power over the sphere, never
/// from the listener-facing sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisymmetricDirectivityTable {
    gain_db: [[f32; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT],
    sphere_averaged_indirect_power_gain: f32,
}

impl AxisymmetricDirectivityTable {
    /// Exact neutral table, including an exact unity indirect-power scalar.
    pub const OMNIDIRECTIONAL: Self = Self {
        gain_db: [[0.0; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT],
        sphere_averaged_indirect_power_gain: 1.0,
    };

    /// Validates and freezes one authored table.
    pub fn new(
        mut gain_db: [[f32; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT],
    ) -> Result<Self, AxisymmetricDirectivityTableError> {
        for (band_index, band) in gain_db.iter_mut().enumerate() {
            for (angle_index, gain) in band.iter_mut().enumerate() {
                if !gain.is_finite() {
                    return Err(AxisymmetricDirectivityTableError::NonFiniteGain {
                        band_index,
                        angle_index,
                    });
                }
                if angle_index == 0 && *gain != 0.0 {
                    return Err(AxisymmetricDirectivityTableError::NonZeroOnAxisGain {
                        band_index,
                    });
                }
                if angle_index != 0 && *gain > 0.0 {
                    return Err(AxisymmetricDirectivityTableError::PositiveOffAxisGain {
                        band_index,
                        angle_index,
                    });
                }
                if *gain == 0.0 {
                    *gain = 0.0;
                }
            }
        }

        let sphere_averaged_indirect_power_gain = sphere_averaged_indirect_power_gain(&gain_db);
        Ok(Self {
            gain_db,
            sphere_averaged_indirect_power_gain,
        })
    }

    /// The complete validated table in band-major, angle-minor order.
    #[must_use]
    pub const fn gain_db(
        &self,
    ) -> [[f32; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT] {
        self.gain_db
    }

    /// Equal-band sphere-averaged linear power relative to on-axis power.
    #[must_use]
    pub const fn sphere_averaged_indirect_power_gain(&self) -> f32 {
        self.sphere_averaged_indirect_power_gain
    }

    /// Evaluates all eight bands at one polar angle using linear dB interpolation.
    pub fn gain_db_at_polar_angle(
        &self,
        polar_angle_degrees: f32,
    ) -> Result<[f32; SPECTRAL_BAND_COUNT], AxisymmetricDirectivityAngleError> {
        if !polar_angle_degrees.is_finite() {
            return Err(AxisymmetricDirectivityAngleError::NonFiniteAngle);
        }
        if !(0.0..=180.0).contains(&polar_angle_degrees) {
            return Err(AxisymmetricDirectivityAngleError::AngleOutOfRange);
        }
        if polar_angle_degrees == 180.0 {
            return Ok(self
                .gain_db
                .map(|band| band[AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT - 1]));
        }

        let node_position =
            f64::from(polar_angle_degrees) / f64::from(AXISYMMETRIC_DIRECTIVITY_ANGLE_STEP_DEGREES);
        let lower_index = node_position.floor() as usize;
        let fraction = node_position - lower_index as f64;
        Ok(self.gain_db.map(|band| {
            let lower = f64::from(band[lower_index]);
            let upper = f64::from(band[lower_index + 1]);
            let interpolated = lower + (upper - lower) * fraction;
            if interpolated == 0.0 {
                0.0
            } else {
                interpolated as f32
            }
        }))
    }
}

impl Default for AxisymmetricDirectivityTable {
    fn default() -> Self {
        Self::OMNIDIRECTIONAL
    }
}

/// Rejected authored table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisymmetricDirectivityTableError {
    NonFiniteGain {
        band_index: usize,
        angle_index: usize,
    },
    NonZeroOnAxisGain {
        band_index: usize,
    },
    PositiveOffAxisGain {
        band_index: usize,
        angle_index: usize,
    },
}

/// Invalid polar angle supplied to the fixed table evaluator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisymmetricDirectivityAngleError {
    NonFiniteAngle,
    AngleOutOfRange,
}

// Eight-point Gauss-Legendre quadrature, paired about each ten-degree
// segment's midpoint. Construction is off the audio callback and happens once
// per attached table.
const GAUSS_LEGENDRE_NODES: [f64; 4] = [
    0.183_434_642_495_649_8,
    0.525_532_409_916_329,
    0.796_666_477_413_626_7,
    0.960_289_856_497_536_3,
];
const GAUSS_LEGENDRE_WEIGHTS: [f64; 4] = [
    0.362_683_783_378_362,
    0.313_706_645_877_887_3,
    0.222_381_034_453_374_5,
    0.101_228_536_290_376_3,
];

fn sphere_averaged_indirect_power_gain(
    gain_db: &[[f32; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT],
) -> f32 {
    if gain_db.iter().flatten().all(|gain| *gain == 0.0) {
        return 1.0;
    }

    let segment_radians =
        core::f64::consts::PI / (AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT.saturating_sub(1)) as f64;
    let half_segment = segment_radians * 0.5;
    let mut band_power_sum = 0.0_f64;

    for band in gain_db {
        let mut angular_integral = 0.0_f64;
        for segment_index in 0..AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT - 1 {
            let segment_start = segment_index as f64 * segment_radians;
            let midpoint = segment_start + half_segment;
            let lower_db = f64::from(band[segment_index]);
            let upper_db = f64::from(band[segment_index + 1]);

            for (node, weight) in GAUSS_LEGENDRE_NODES
                .iter()
                .copied()
                .zip(GAUSS_LEGENDRE_WEIGHTS.iter().copied())
            {
                for sign in [-1.0_f64, 1.0] {
                    let theta = midpoint + sign * half_segment * node;
                    let fraction = (theta - segment_start) / segment_radians;
                    let interpolated_db = lower_db + (upper_db - lower_db) * fraction;
                    let power_gain = 10.0_f64.powf(interpolated_db / 10.0);
                    angular_integral += half_segment * weight * power_gain * theta.sin();
                }
            }
        }
        band_power_sum += 0.5 * angular_integral;
    }

    (band_power_sum / SPECTRAL_BAND_COUNT as f64).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_rear_loss_table(
        rear_loss_db: [f32; SPECTRAL_BAND_COUNT],
    ) -> [[f32; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT] {
        core::array::from_fn(|band_index| {
            core::array::from_fn(|angle_index| {
                rear_loss_db[band_index] * angle_index as f32
                    / (AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT - 1) as f32
            })
        })
    }

    #[test]
    fn representation_is_the_ratified_compact_shape_and_omni_is_exact() {
        assert_eq!(AXISYMMETRIC_DIRECTIVITY_ANGLES_DEGREES[0], 0.0);
        assert_eq!(AXISYMMETRIC_DIRECTIVITY_ANGLES_DEGREES[18], 180.0);
        assert_eq!(core::mem::size_of::<AxisymmetricDirectivityTable>(), 612);

        let table = AxisymmetricDirectivityTable::default();
        assert_eq!(
            table.gain_db(),
            [[0.0; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT]
        );
        assert_eq!(table.sphere_averaged_indirect_power_gain(), 1.0);
        assert_eq!(table.gain_db_at_polar_angle(73.25).unwrap(), [0.0; 8]);
    }

    #[test]
    fn validation_rejects_nonfinite_nonzero_front_and_positive_off_axis_nodes() {
        let mut nonfinite = [[0.0; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT];
        nonfinite[3][7] = f32::NAN;
        assert_eq!(
            AxisymmetricDirectivityTable::new(nonfinite),
            Err(AxisymmetricDirectivityTableError::NonFiniteGain {
                band_index: 3,
                angle_index: 7,
            })
        );

        let mut nonzero_front = [[0.0; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT];
        nonzero_front[5][0] = -0.01;
        assert_eq!(
            AxisymmetricDirectivityTable::new(nonzero_front),
            Err(AxisymmetricDirectivityTableError::NonZeroOnAxisGain { band_index: 5 })
        );

        let mut positive = [[0.0; AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT]; SPECTRAL_BAND_COUNT];
        positive[1][18] = 0.01;
        assert_eq!(
            AxisymmetricDirectivityTable::new(positive),
            Err(AxisymmetricDirectivityTableError::PositiveOffAxisGain {
                band_index: 1,
                angle_index: 18,
            })
        );
    }

    #[test]
    fn interpolation_is_node_exact_and_linear_in_db() {
        let table = AxisymmetricDirectivityTable::new(linear_rear_loss_table([
            -1.0, -2.0, -3.0, -4.0, -5.0, -6.0, -7.0, -8.0,
        ]))
        .unwrap();

        assert_eq!(
            table.gain_db_at_polar_angle(90.0).unwrap(),
            [-0.5, -1.0, -1.5, -2.0, -2.5, -3.0, -3.5, -4.0]
        );
        assert!((table.gain_db_at_polar_angle(95.0).unwrap()[5] + 3.166_666_7).abs() < 1.0e-6);
        assert_eq!(table.gain_db_at_polar_angle(180.0).unwrap()[7], -8.0);
        assert_eq!(
            table.gain_db_at_polar_angle(f32::NAN),
            Err(AxisymmetricDirectivityAngleError::NonFiniteAngle)
        );
        assert_eq!(
            table.gain_db_at_polar_angle(180.1),
            Err(AxisymmetricDirectivityAngleError::AngleOutOfRange)
        );
    }

    #[test]
    fn sphere_average_matches_a_known_linear_db_polar_law() {
        let table = AxisymmetricDirectivityTable::new(linear_rear_loss_table([-6.0; 8])).unwrap();
        let exponent_per_radian = 6.0 * 10.0_f64.ln() / (10.0 * core::f64::consts::PI);
        let expected = 0.5 * (1.0 + (-exponent_per_radian * core::f64::consts::PI).exp())
            / (1.0 + exponent_per_radian * exponent_per_radian);
        let actual = f64::from(table.sphere_averaged_indirect_power_gain());
        assert!(
            (actual - expected).abs() < 1.0e-6,
            "expected {expected:.9}, got {actual:.9}"
        );
        assert!(actual > 0.0 && actual < 1.0);
    }
}
