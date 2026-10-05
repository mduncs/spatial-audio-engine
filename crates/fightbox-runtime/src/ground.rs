//! Smooth statistical ground coloration for eligible outdoor paths.
//!
//! The model turns an authored hard-to-porous factor into one bounded,
//! continuous octave correction. It intentionally has no delay state and makes
//! no coherent-interference claim; the resulting curve is published into the
//! shared [`SpectralStage::Ground`] contribution and filtered exactly once.

use fightbox_api::ground::{
    GroundAuthoringPolicy, GroundSourceKind, StatisticalGroundRequest,
    StatisticalGroundRequestError,
};
use fightbox_api::spectral::{
    SPECTRAL_BAND_COUNT, SpectralStage, SpectralTransfer, SpectralTransferError,
};

/// Full-strength porous-ground coloration before distance and height weighting.
///
/// This is a V1 statistical artistic curve informed by the ISO 9613-2/CNOSSOS
/// ground-factor convention. It is not the standardized engineering method and
/// must not be reported as a regulatory noise prediction.
pub const FULL_POROUS_GROUND_GAIN_DB: [f32; SPECTRAL_BAND_COUNT] =
    [-0.1, -0.3, -1.0, -2.4, -4.0, -5.5, -6.0, -6.0];

/// Why one request did or did not contribute ground coloration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroundApplication {
    AppliedDefault,
    AppliedNonNormativeOverride,
    NeutralHardGround,
    NeutralZeroDistance,
    ExcludedByAuthoring,
    ExcludedSourceKind,
    ExcludedObstructed,
    ExcludedDiffractionAuthority,
}

/// One evaluated ground contribution plus stable eligibility provenance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatisticalGroundResult {
    pub application: GroundApplication,
    pub stage_gain_db: [f32; SPECTRAL_BAND_COUNT],
}

impl StatisticalGroundResult {
    #[must_use]
    pub fn applied(self) -> bool {
        matches!(
            self.application,
            GroundApplication::AppliedDefault | GroundApplication::AppliedNonNormativeOverride
        )
    }
}

/// Stateless V1 statistical ground evaluator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatisticalGroundModel;

impl StatisticalGroundModel {
    /// Evaluates one segment without mutating a composed transfer.
    pub fn evaluate(
        request: StatisticalGroundRequest,
    ) -> Result<StatisticalGroundResult, StatisticalGroundTransferError> {
        request
            .validate()
            .map_err(StatisticalGroundTransferError::InvalidRequest)?;
        let neutral = |application| StatisticalGroundResult {
            application,
            stage_gain_db: [0.0; SPECTRAL_BAND_COUNT],
        };

        // Diffraction owns the obstructed segment regardless of an authoring
        // override. Applying both would double-count the same terrain event.
        if request.diffraction_applied {
            return Ok(neutral(GroundApplication::ExcludedDiffractionAuthority));
        }
        if request.authoring == GroundAuthoringPolicy::ForceOff {
            return Ok(neutral(GroundApplication::ExcludedByAuthoring));
        }
        if !request.unobstructed {
            return Ok(neutral(GroundApplication::ExcludedObstructed));
        }
        if request.ground_factor == 0.0 {
            return Ok(neutral(GroundApplication::NeutralHardGround));
        }
        if request.distance_m == 0.0 {
            return Ok(neutral(GroundApplication::NeutralZeroDistance));
        }

        let application = match (request.source_kind, request.authoring) {
            (GroundSourceKind::SteadyGroundBound, GroundAuthoringPolicy::Default) => {
                GroundApplication::AppliedDefault
            }
            (_, GroundAuthoringPolicy::ForceOnNonNormative) => {
                GroundApplication::AppliedNonNormativeOverride
            }
            _ => return Ok(neutral(GroundApplication::ExcludedSourceKind)),
        };

        // Saturate smoothly with path length while shrinking continuously as
        // either endpoint rises. This avoids the moving-source zipper and comb
        // behavior of a coherent image-source reflection.
        let distance_weight = 1.0_f64 - (-f64::from(request.distance_m) / 60.0).exp();
        let height_sum_m = f64::from(request.source_height_m + request.listener_height_m);
        let height_weight = 1.0 / (1.0 + height_sum_m / 3.0);
        let strength = f64::from(request.ground_factor) * distance_weight * height_weight;
        let stage_gain_db = FULL_POROUS_GROUND_GAIN_DB.map(|full_gain_db| {
            let gain_db = f64::from(full_gain_db) * strength;
            if gain_db == 0.0 { 0.0 } else { gain_db as f32 }
        });
        Ok(StatisticalGroundResult {
            application,
            stage_gain_db,
        })
    }

    /// Replaces only the named Ground stage, clearing stale coloration when a
    /// moving path becomes ineligible.
    pub fn publish(
        request: StatisticalGroundRequest,
        transfer: &mut SpectralTransfer,
    ) -> Result<GroundApplication, StatisticalGroundTransferError> {
        let result = Self::evaluate(request)?;
        transfer
            .set_stage(SpectralStage::Ground, result.stage_gain_db)
            .map_err(StatisticalGroundTransferError::SpectralTransfer)?;
        Ok(result.application)
    }
}

/// Failure while producing or publishing one ground contribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatisticalGroundTransferError {
    InvalidRequest(StatisticalGroundRequestError),
    SpectralTransfer(SpectralTransferError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpectralTransferFilter;

    fn request(factor: f32) -> StatisticalGroundRequest {
        StatisticalGroundRequest::steady_ground_bound(factor, 100.0, 0.5, 1.5)
    }

    #[test]
    fn porous_ground_darkens_smoothly_while_hard_ground_is_neutral() {
        let hard = StatisticalGroundModel::evaluate(request(0.0)).unwrap();
        let mixed = StatisticalGroundModel::evaluate(request(0.5)).unwrap();
        let porous = StatisticalGroundModel::evaluate(request(1.0)).unwrap();
        assert_eq!(hard.application, GroundApplication::NeutralHardGround);
        assert_eq!(hard.stage_gain_db, [0.0; SPECTRAL_BAND_COUNT]);
        assert_eq!(mixed.application, GroundApplication::AppliedDefault);
        for band_index in 0..SPECTRAL_BAND_COUNT {
            assert!(porous.stage_gain_db[band_index] <= mixed.stage_gain_db[band_index]);
            assert!(
                (mixed.stage_gain_db[band_index] * 2.0 - porous.stage_gain_db[band_index]).abs()
                    < 1.0e-6
            );
        }
        assert!(porous.stage_gain_db[5] < porous.stage_gain_db[0] - 2.0);
    }

    #[test]
    fn event_defaults_and_diffraction_are_excluded_but_override_is_visible() {
        for source_kind in [
            GroundSourceKind::Airborne,
            GroundSourceKind::BallisticCrack,
            GroundSourceKind::Thunder,
            GroundSourceKind::Firework,
            GroundSourceKind::Explosion,
        ] {
            let excluded = StatisticalGroundModel::evaluate(StatisticalGroundRequest {
                source_kind,
                ..request(1.0)
            })
            .unwrap();
            assert_eq!(excluded.application, GroundApplication::ExcludedSourceKind);
            assert_eq!(excluded.stage_gain_db, [0.0; SPECTRAL_BAND_COUNT]);
        }

        let overridden = StatisticalGroundModel::evaluate(StatisticalGroundRequest {
            source_kind: GroundSourceKind::Firework,
            authoring: GroundAuthoringPolicy::ForceOnNonNormative,
            ..request(1.0)
        })
        .unwrap();
        assert_eq!(
            overridden.application,
            GroundApplication::AppliedNonNormativeOverride
        );
        assert!(overridden.applied());

        let diffraction = StatisticalGroundModel::evaluate(StatisticalGroundRequest {
            source_kind: GroundSourceKind::Firework,
            authoring: GroundAuthoringPolicy::ForceOnNonNormative,
            diffraction_applied: true,
            ..request(1.0)
        })
        .unwrap();
        assert_eq!(
            diffraction.application,
            GroundApplication::ExcludedDiffractionAuthority
        );
        assert_eq!(diffraction.stage_gain_db, [0.0; SPECTRAL_BAND_COUNT]);
    }

    #[test]
    fn publication_clears_stale_ground_without_touching_other_stages() {
        let mut transfer = SpectralTransfer::NEUTRAL;
        transfer
            .set_stage(SpectralStage::Atmosphere, [-1.0; SPECTRAL_BAND_COUNT])
            .unwrap();
        assert_eq!(
            StatisticalGroundModel::publish(request(1.0), &mut transfer).unwrap(),
            GroundApplication::AppliedDefault
        );
        assert_ne!(
            transfer.stage_gain_db(SpectralStage::Ground),
            [0.0; SPECTRAL_BAND_COUNT]
        );
        assert_eq!(
            StatisticalGroundModel::publish(
                StatisticalGroundRequest {
                    unobstructed: false,
                    ..request(1.0)
                },
                &mut transfer,
            )
            .unwrap(),
            GroundApplication::ExcludedObstructed
        );
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Ground),
            [0.0; SPECTRAL_BAND_COUNT]
        );
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Atmosphere),
            [-1.0; SPECTRAL_BAND_COUNT]
        );
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
    fn grass_is_audibly_darker_than_hard_ground_without_a_comb_path() {
        let mut hard = SpectralTransfer::NEUTRAL;
        let mut grass = SpectralTransfer::NEUTRAL;
        StatisticalGroundModel::publish(request(0.0), &mut hard).unwrap();
        StatisticalGroundModel::publish(request(1.0), &mut grass).unwrap();
        let hard_125 = filtered_tone_rms(hard, 125.0);
        let hard_4k = filtered_tone_rms(hard, 4_000.0);
        let grass_125 = filtered_tone_rms(grass, 125.0);
        let grass_4k = filtered_tone_rms(grass, 4_000.0);
        println!(
            "GROUND_SMOKE hard_125={hard_125:.6} hard_4k={hard_4k:.6} grass_125={grass_125:.6} grass_4k={grass_4k:.6}"
        );
        assert!(grass_125 > hard_125 * 0.90);
        assert!(grass_4k < hard_4k * 0.80);
    }
}
