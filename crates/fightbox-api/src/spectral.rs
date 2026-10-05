//! Vendor-neutral composition contract for environmental spectral color.
//!
//! Every propagation module contributes control-rate changes in decibels to
//! the same eight octave bands. [`SpectralTransfer`] preserves those named
//! contributions for diagnostics, then produces one bounded combined curve
//! for the runtime's sole filter application.

/// Number of octave bands in the V1 environmental transfer contract.
pub const SPECTRAL_BAND_COUNT: usize = 8;

/// Octave-band centres, in ascending frequency order.
pub const SPECTRAL_BAND_CENTERS_HZ: [f32; SPECTRAL_BAND_COUNT] = [
    125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
];

/// Combined attenuation floor. Lower physical contributions remain visible in
/// their stage stem, but the runtime filter never receives a smaller gain.
pub const MIN_COMBINED_SPECTRAL_GAIN_DB: f32 = -120.0;

/// Combined boost ceiling. This admits bounded statistical-ground or enclosure
/// coloration without allowing several modules to multiply into an unsafe gain.
pub const MAX_COMBINED_SPECTRAL_GAIN_DB: f32 = 12.0;

/// Named contributors to the one composed environmental transfer.
///
/// Discriminants are array indices and therefore part of the deterministic
/// composition order. New contributors require an explicit contract revision.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpectralStage {
    Directivity = 0,
    Atmosphere = 1,
    Ground = 2,
    Occlusion = 3,
    Enclosure = 4,
}

impl SpectralStage {
    pub const COUNT: usize = 5;
    pub const ALL: [Self; Self::COUNT] = [
        Self::Directivity,
        Self::Atmosphere,
        Self::Ground,
        Self::Occlusion,
        Self::Enclosure,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Invalid stage contribution supplied to [`SpectralTransfer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpectralTransferError {
    NonFiniteBand {
        stage: SpectralStage,
        band_index: usize,
    },
}

/// The complete, fixed-size environmental coloration for one signal path.
///
/// Stage values are amplitude changes in dB. They may exceed the final floor
/// (for example, upper-band atmosphere loss over 10 km) so diagnostics retain
/// the physical contribution. Composition always visits [`SpectralStage::ALL`]
/// in order, accumulates in `f64`, and clamps the one final curve before
/// converting it to linear amplitude gains.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectralTransfer {
    stage_gain_db: [[f32; SPECTRAL_BAND_COUNT]; SpectralStage::COUNT],
    combined_gain_db: [f32; SPECTRAL_BAND_COUNT],
    combined_linear_gain: [f32; SPECTRAL_BAND_COUNT],
}

impl SpectralTransfer {
    pub const NEUTRAL: Self = Self {
        stage_gain_db: [[0.0; SPECTRAL_BAND_COUNT]; SpectralStage::COUNT],
        combined_gain_db: [0.0; SPECTRAL_BAND_COUNT],
        combined_linear_gain: [1.0; SPECTRAL_BAND_COUNT],
    };

    /// Replaces one named stage and recomposes the final curve.
    ///
    /// Validation completes before mutation, so an error leaves `self`
    /// unchanged. Signed zero is canonicalized to positive zero, preserving one
    /// exact representation of the neutral transfer.
    pub fn set_stage(
        &mut self,
        stage: SpectralStage,
        gain_db: [f32; SPECTRAL_BAND_COUNT],
    ) -> Result<(), SpectralTransferError> {
        for (band_index, gain) in gain_db.iter().copied().enumerate() {
            if !gain.is_finite() {
                return Err(SpectralTransferError::NonFiniteBand { stage, band_index });
            }
        }

        self.stage_gain_db[stage.index()] =
            gain_db.map(|gain| if gain == 0.0 { 0.0 } else { gain });
        self.recompose();
        Ok(())
    }

    /// Builder-style form of [`Self::set_stage`].
    pub fn with_stage(
        mut self,
        stage: SpectralStage,
        gain_db: [f32; SPECTRAL_BAND_COUNT],
    ) -> Result<Self, SpectralTransferError> {
        self.set_stage(stage, gain_db)?;
        Ok(self)
    }

    /// Removes one contributor without disturbing the other stage stems.
    pub fn clear_stage(&mut self, stage: SpectralStage) {
        self.stage_gain_db[stage.index()] = [0.0; SPECTRAL_BAND_COUNT];
        self.recompose();
    }

    /// The un-clamped contribution from one named stage, for diagnostics and
    /// isolated stage-stem rendering.
    #[must_use]
    pub const fn stage_gain_db(&self, stage: SpectralStage) -> [f32; SPECTRAL_BAND_COUNT] {
        self.stage_gain_db[stage.index()]
    }

    /// The bounded dB curve supplied to the one runtime filter.
    #[must_use]
    pub const fn combined_gain_db(&self) -> [f32; SPECTRAL_BAND_COUNT] {
        self.combined_gain_db
    }

    /// Precomputed linear amplitude gains for realtime filtering.
    #[must_use]
    pub const fn combined_linear_gain(&self) -> [f32; SPECTRAL_BAND_COUNT] {
        self.combined_linear_gain
    }

    /// True when the composed final curve is canonical 0 dB in every band.
    /// Exactly cancelling visible stage contributions therefore bypass too.
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        self.combined_gain_db
            .iter()
            .all(|gain| gain.to_bits() == 0.0_f32.to_bits())
    }

    fn recompose(&mut self) {
        for band_index in 0..SPECTRAL_BAND_COUNT {
            let mut sum_db = 0.0_f64;
            for stage in SpectralStage::ALL {
                sum_db += f64::from(self.stage_gain_db[stage.index()][band_index]);
            }
            let bounded_db = sum_db.clamp(
                f64::from(MIN_COMBINED_SPECTRAL_GAIN_DB),
                f64::from(MAX_COMBINED_SPECTRAL_GAIN_DB),
            ) as f32;
            let canonical_db = if bounded_db == 0.0 { 0.0 } else { bounded_db };
            self.combined_gain_db[band_index] = canonical_db;
            self.combined_linear_gain[band_index] = if canonical_db == 0.0 {
                1.0
            } else {
                10.0_f32.powf(canonical_db / 20.0)
            };
        }
    }
}

impl Default for SpectralTransfer {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_transfer_has_one_canonical_exact_representation() {
        let mut transfer = SpectralTransfer::default();
        transfer
            .set_stage(SpectralStage::Ground, [-0.0; SPECTRAL_BAND_COUNT])
            .unwrap();

        assert!(transfer.is_neutral());
        for stage in SpectralStage::ALL {
            assert!(
                transfer
                    .stage_gain_db(stage)
                    .into_iter()
                    .all(|gain| gain.to_bits() == 0.0_f32.to_bits())
            );
        }
        assert!(
            transfer
                .combined_linear_gain()
                .into_iter()
                .all(|gain| gain.to_bits() == 1.0_f32.to_bits())
        );
    }

    #[test]
    fn composition_is_order_independent_and_stage_stems_remain_visible() {
        let directivity = [-1.0, -1.5, -2.0, -2.5, -3.0, -4.0, -5.0, -6.0];
        let atmosphere = [-0.1, -0.2, -0.4, -0.8, -1.6, -3.2, -6.4, -12.8];
        let ground = [1.0, 0.5, 0.0, -0.5, -1.0, -1.5, -2.0, -2.5];

        let forward = SpectralTransfer::default()
            .with_stage(SpectralStage::Directivity, directivity)
            .unwrap()
            .with_stage(SpectralStage::Atmosphere, atmosphere)
            .unwrap()
            .with_stage(SpectralStage::Ground, ground)
            .unwrap();
        let reverse = SpectralTransfer::default()
            .with_stage(SpectralStage::Ground, ground)
            .unwrap()
            .with_stage(SpectralStage::Atmosphere, atmosphere)
            .unwrap()
            .with_stage(SpectralStage::Directivity, directivity)
            .unwrap();

        assert_eq!(forward, reverse);
        assert_eq!(forward.stage_gain_db(SpectralStage::Atmosphere), atmosphere);
        for (band_index, combined) in forward.combined_gain_db().into_iter().enumerate() {
            let expected = (f64::from(directivity[band_index])
                + f64::from(atmosphere[band_index])
                + f64::from(ground[band_index])) as f32;
            assert_eq!(combined.to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn final_curve_is_finite_and_bounded_even_for_extreme_finite_stages() {
        let transfer = SpectralTransfer::default()
            .with_stage(SpectralStage::Atmosphere, [-1_000.0; SPECTRAL_BAND_COUNT])
            .unwrap()
            .with_stage(SpectralStage::Ground, [100.0; SPECTRAL_BAND_COUNT])
            .unwrap();
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Atmosphere),
            [-1_000.0; SPECTRAL_BAND_COUNT]
        );
        assert_eq!(
            transfer.combined_gain_db(),
            [MIN_COMBINED_SPECTRAL_GAIN_DB; SPECTRAL_BAND_COUNT]
        );
        assert!(
            transfer.combined_linear_gain().into_iter().all(|gain| {
                gain.is_finite() && gain > 0.0 && gain <= 10.0_f32.powf(12.0 / 20.0)
            })
        );

        let boosted = SpectralTransfer::default()
            .with_stage(SpectralStage::Ground, [100.0; SPECTRAL_BAND_COUNT])
            .unwrap();
        assert_eq!(
            boosted.combined_gain_db(),
            [MAX_COMBINED_SPECTRAL_GAIN_DB; SPECTRAL_BAND_COUNT]
        );
    }

    #[test]
    fn rejected_nonfinite_stage_does_not_partially_mutate_the_transfer() {
        let mut transfer = SpectralTransfer::default();
        let before = transfer;
        let mut invalid = [0.0; SPECTRAL_BAND_COUNT];
        invalid[3] = f32::NAN;

        assert_eq!(
            transfer.set_stage(SpectralStage::Occlusion, invalid),
            Err(SpectralTransferError::NonFiniteBand {
                stage: SpectralStage::Occlusion,
                band_index: 3,
            })
        );
        assert_eq!(transfer, before);
    }
}
