//! ISO 9613-1 atmospheric absorption frozen for one acoustic session.
//!
//! Coefficients are generated once from the host observation. Distance then
//! supplies a control-rate dB contribution to [`SpectralStage::Atmosphere`].
//! This module deliberately exposes no sound-speed calculation: weather changes
//! tone only, while engine propagation timing remains fixed by `EngineConfig`.

use fightbox_api::atmosphere::{
    AtmosphereObservation, AtmosphereProvenance, FALLBACK_ATMOSPHERE_OBSERVATION,
};
use fightbox_api::spectral::{
    SPECTRAL_BAND_CENTERS_HZ, SPECTRAL_BAND_COUNT, SpectralStage, SpectralTransfer,
    SpectralTransferError,
};

const REFERENCE_TEMPERATURE_K: f64 = 293.15;
const TRIPLE_POINT_TEMPERATURE_K: f64 = 273.16;
const REFERENCE_PRESSURE_KPA: f64 = 101.325;
const CELSIUS_TO_KELVIN: f64 = 273.15;
const DECIBELS_PER_NEPER: f64 = 8.686;

/// Representative frequencies that reduce ISO 9613-1 to a three-band EQ split
/// at 0.8 and 8 kHz (Steam Audio's 0–0.8 / 0.8–8 / 8–22 kHz bands).
pub const THREE_BAND_AIR_REFERENCE_HZ: [f32; 3] = [400.0, 2_530.0, 13_266.0];

/// Fallback-atmosphere ISO 9613-1 pressure exponents β per metre at
/// [`THREE_BAND_AIR_REFERENCE_HZ`]: a band's pressure gain over `L` metres is
/// `exp(-β·L)`. The single shared air law for every three-band stage.
pub const FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M: [f32; 3] =
    [0.000_257_764_2, 0.001_599_23, 0.030_389_35];

/// Frozen session atmosphere plus its eight pure-tone absorption coefficients.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrozenAtmosphere {
    observation: AtmosphereObservation,
    provenance: AtmosphereProvenance,
    absorption_db_per_meter: [f64; SPECTRAL_BAND_COUNT],
}

impl FrozenAtmosphere {
    /// Freezes a valid host observation or selects the deterministic fallback.
    ///
    /// Invalid input does not leak partially accepted weather into the session;
    /// the complete fallback observation and a stable reason are retained.
    #[must_use]
    pub fn freeze(host_observation: Option<AtmosphereObservation>) -> Self {
        let (observation, provenance) = match host_observation {
            Some(observation) => match observation.validate() {
                Ok(()) => (observation, AtmosphereProvenance::HostProvided),
                Err(error) => (
                    FALLBACK_ATMOSPHERE_OBSERVATION,
                    AtmosphereProvenance::DeterministicFallbackInvalid(error),
                ),
            },
            None => (
                FALLBACK_ATMOSPHERE_OBSERVATION,
                AtmosphereProvenance::DeterministicFallbackMissing,
            ),
        };
        Self {
            observation,
            provenance,
            absorption_db_per_meter: iso_9613_1_absorption_db_per_meter(observation),
        }
    }

    #[must_use]
    pub const fn observation(&self) -> AtmosphereObservation {
        self.observation
    }

    #[must_use]
    pub const fn provenance(&self) -> AtmosphereProvenance {
        self.provenance
    }

    /// Pure-tone absorption at the eight fixed band centres, in dB/m.
    #[must_use]
    pub const fn absorption_db_per_meter(&self) -> [f64; SPECTRAL_BAND_COUNT] {
        self.absorption_db_per_meter
    }

    /// Pressure exponents for the shared direct, routed and echo air law.
    #[must_use]
    pub fn three_band_air_pressure_exponents_per_m(&self) -> [f32; 3] {
        if self.observation == FALLBACK_ATMOSPHERE_OBSERVATION {
            return FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M;
        }
        iso_9613_1_absorption_db_per_meter_at(self.observation, THREE_BAND_AIR_REFERENCE_HZ)
            .map(|coefficient| (coefficient / DECIBELS_PER_NEPER) as f32)
    }

    /// Builds the un-clamped Atmosphere stage at one physical path distance.
    ///
    /// The values are negative amplitude changes. Long-distance upper-band
    /// losses may be much lower than the composed filter floor so the visible
    /// stage stem retains the physical result.
    pub fn stage_gain_db_at_distance(
        &self,
        distance_m: f32,
    ) -> Result<[f32; SPECTRAL_BAND_COUNT], AtmosphereTransferError> {
        if !distance_m.is_finite() || distance_m < 0.0 {
            return Err(AtmosphereTransferError::InvalidDistance);
        }
        let distance_m = f64::from(distance_m);
        let mut stage = [0.0_f32; SPECTRAL_BAND_COUNT];
        for (band_index, coefficient) in self.absorption_db_per_meter.iter().copied().enumerate() {
            let loss_db = coefficient * distance_m;
            if !loss_db.is_finite() || loss_db > f64::from(f32::MAX) {
                return Err(AtmosphereTransferError::UnrepresentableLoss { band_index });
            }
            stage[band_index] = if loss_db == 0.0 {
                0.0
            } else {
                -(loss_db as f32)
            };
        }
        Ok(stage)
    }

    /// Publishes this session's distance loss into the named Atmosphere stem.
    /// Other spectral contributors remain untouched.
    pub fn publish_at_distance(
        &self,
        distance_m: f32,
        transfer: &mut SpectralTransfer,
    ) -> Result<(), AtmosphereTransferError> {
        let stage = self.stage_gain_db_at_distance(distance_m)?;
        transfer
            .set_stage(SpectralStage::Atmosphere, stage)
            .map_err(AtmosphereTransferError::SpectralTransfer)
    }
}

/// Failure while converting a frozen atmosphere into one transfer stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtmosphereTransferError {
    InvalidDistance,
    UnrepresentableLoss { band_index: usize },
    SpectralTransfer(SpectralTransferError),
}

fn iso_9613_1_absorption_db_per_meter(
    observation: AtmosphereObservation,
) -> [f64; SPECTRAL_BAND_COUNT] {
    iso_9613_1_absorption_db_per_meter_at(observation, SPECTRAL_BAND_CENTERS_HZ)
}

fn iso_9613_1_absorption_db_per_meter_at<const N: usize>(
    observation: AtmosphereObservation,
    frequencies_hz: [f32; N],
) -> [f64; N] {
    let temperature_k = f64::from(observation.temperature_c) + CELSIUS_TO_KELVIN;
    let pressure_kpa = f64::from(observation.pressure_kpa);
    let pressure_ratio = pressure_kpa / REFERENCE_PRESSURE_KPA;
    let temperature_ratio = temperature_k / REFERENCE_TEMPERATURE_K;

    let saturation_pressure_ratio =
        10.0_f64.powf(-6.8346 * (TRIPLE_POINT_TEMPERATURE_K / temperature_k).powf(1.261) + 4.6151);
    let water_vapor_molar_concentration_percent = f64::from(observation.relative_humidity_percent)
        * saturation_pressure_ratio
        / pressure_ratio;
    let humidity = water_vapor_molar_concentration_percent;
    let oxygen_relaxation_hz =
        pressure_ratio * (24.0 + 4.04e4 * humidity * (0.02 + humidity) / (0.391 + humidity));
    let nitrogen_relaxation_hz = pressure_ratio
        * temperature_ratio.powf(-0.5)
        * (9.0 + 280.0 * humidity * (-4.170 * (temperature_ratio.powf(-1.0 / 3.0) - 1.0)).exp());

    frequencies_hz.map(|frequency_hz| {
        let frequency_hz = f64::from(frequency_hz);
        let frequency_squared = frequency_hz * frequency_hz;
        let classical = 1.84e-11 * pressure_ratio.recip() * temperature_ratio.sqrt();
        let oxygen = 0.01275 * (-2239.1 / temperature_k).exp()
            / (oxygen_relaxation_hz + frequency_squared / oxygen_relaxation_hz);
        let nitrogen = 0.1068 * (-3352.0 / temperature_k).exp()
            / (nitrogen_relaxation_hz + frequency_squared / nitrogen_relaxation_hz);
        DECIBELS_PER_NEPER
            * frequency_squared
            * (classical + temperature_ratio.powf(-2.5) * (oxygen + nitrogen))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpectralTransferFilter;
    use fightbox_api::atmosphere::{
        AtmosphereObservationError, MAX_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT,
        MAX_ATMOSPHERE_TEMPERATURE_C, MIN_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT,
        MIN_ATMOSPHERE_TEMPERATURE_C,
    };

    #[test]
    fn reference_coefficients_match_the_wave_17_plan_values() {
        let atmosphere = FrozenAtmosphere::freeze(None);
        let expected_db_per_km = [
            0.439_790,
            1.309_750,
            2.728_134,
            4.664_732,
            9.887_016,
            29.665_528,
            105.290_926,
            364.541_021,
        ];
        for (band_index, coefficient) in
            atmosphere.absorption_db_per_meter().into_iter().enumerate()
        {
            let actual_db_per_km = coefficient * 1_000.0;
            assert!(
                (actual_db_per_km - expected_db_per_km[band_index]).abs() < 0.002,
                "band {band_index} produced {actual_db_per_km:.6} dB/km"
            );
        }
    }

    #[test]
    fn three_band_air_exponents_are_the_fallback_iso_model() {
        assert_eq!(
            FrozenAtmosphere::freeze(None)
                .three_band_air_pressure_exponents_per_m()
                .map(f32::to_bits),
            FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M.map(f32::to_bits)
        );
        let db_per_meter = iso_9613_1_absorption_db_per_meter_at(
            FALLBACK_ATMOSPHERE_OBSERVATION,
            THREE_BAND_AIR_REFERENCE_HZ,
        );
        for (band, exponent) in FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M
            .into_iter()
            .enumerate()
        {
            let expected = db_per_meter[band] / DECIBELS_PER_NEPER;
            assert!(
                (f64::from(exponent) / expected - 1.0).abs() < 1.0e-5,
                "band {band}: {exponent} per m, ISO {expected} per m"
            );
        }
    }

    #[test]
    fn valid_envelope_coefficients_are_positive_finite_and_host_provenanced() {
        for observation in [
            AtmosphereObservation {
                temperature_c: MIN_ATMOSPHERE_TEMPERATURE_C,
                relative_humidity_percent: MIN_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT,
                pressure_kpa: 80.0,
            },
            AtmosphereObservation {
                temperature_c: MAX_ATMOSPHERE_TEMPERATURE_C,
                relative_humidity_percent: MAX_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT,
                pressure_kpa: 105.0,
            },
        ] {
            let frozen = FrozenAtmosphere::freeze(Some(observation));
            assert_eq!(frozen.provenance(), AtmosphereProvenance::HostProvided);
            assert!(
                frozen
                    .absorption_db_per_meter()
                    .into_iter()
                    .all(|coefficient| coefficient.is_finite() && coefficient > 0.0)
            );
        }
    }

    #[test]
    fn missing_and_invalid_weather_choose_identical_deterministic_coefficients() {
        let missing = FrozenAtmosphere::freeze(None);
        let invalid = FrozenAtmosphere::freeze(Some(AtmosphereObservation {
            pressure_kpa: f32::NAN,
            ..FALLBACK_ATMOSPHERE_OBSERVATION
        }));

        assert_eq!(missing.observation(), FALLBACK_ATMOSPHERE_OBSERVATION);
        assert_eq!(invalid.observation(), FALLBACK_ATMOSPHERE_OBSERVATION);
        assert_eq!(
            missing.provenance(),
            AtmosphereProvenance::DeterministicFallbackMissing
        );
        assert_eq!(
            invalid.provenance(),
            AtmosphereProvenance::DeterministicFallbackInvalid(
                AtmosphereObservationError::NonFinitePressure
            )
        );
        assert_eq!(
            missing.absorption_db_per_meter().map(f64::to_bits),
            invalid.absorption_db_per_meter().map(f64::to_bits)
        );
    }

    #[test]
    fn distance_publication_is_neutral_at_zero_and_replaces_only_atmosphere() {
        let atmosphere = FrozenAtmosphere::freeze(None);
        let mut transfer = SpectralTransfer::NEUTRAL;
        atmosphere.publish_at_distance(0.0, &mut transfer).unwrap();
        assert!(transfer.is_neutral());
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Atmosphere),
            [0.0; SPECTRAL_BAND_COUNT]
        );

        transfer
            .set_stage(SpectralStage::Ground, [-3.0; SPECTRAL_BAND_COUNT])
            .unwrap();
        atmosphere
            .publish_at_distance(1_000.0, &mut transfer)
            .unwrap();
        let one_km = transfer.stage_gain_db(SpectralStage::Atmosphere);
        assert!((one_km[0] + 0.44).abs() < 0.01);
        assert!((one_km[2] + 2.73).abs() < 0.01);
        assert!((one_km[3] + 4.66).abs() < 0.01);
        assert!((one_km[4] + 9.89).abs() < 0.01);
        assert!((one_km[5] + 29.67).abs() < 0.01);
        assert_eq!(
            transfer.stage_gain_db(SpectralStage::Ground),
            [-3.0; SPECTRAL_BAND_COUNT]
        );

        atmosphere
            .publish_at_distance(10_000.0, &mut transfer)
            .unwrap();
        let ten_km = transfer.stage_gain_db(SpectralStage::Atmosphere);
        assert!((ten_km[0] + 4.40).abs() < 0.01);
        assert!((ten_km[2] + 27.28).abs() < 0.02);
        assert!((ten_km[3] + 46.65).abs() < 0.02);
        assert!((ten_km[4] + 98.87).abs() < 0.02);
        assert!((ten_km[5] + 296.66).abs() < 0.05);
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
    fn one_and_ten_kilometre_wash_keeps_body_while_progressively_losing_crack() {
        let atmosphere = FrozenAtmosphere::freeze(None);
        let mut one_km = SpectralTransfer::NEUTRAL;
        let mut ten_km = SpectralTransfer::NEUTRAL;
        atmosphere
            .publish_at_distance(1_000.0, &mut one_km)
            .unwrap();
        atmosphere
            .publish_at_distance(10_000.0, &mut ten_km)
            .unwrap();

        let one_km_body = filtered_tone_rms(one_km, 125.0);
        let one_km_crack = filtered_tone_rms(one_km, 4_000.0);
        let ten_km_body = filtered_tone_rms(ten_km, 125.0);
        let ten_km_crack = filtered_tone_rms(ten_km, 4_000.0);
        println!(
            "ATMOSPHERE_WASH one_km_body_125_rms={one_km_body:.6} one_km_crack_4000_rms={one_km_crack:.6} ten_km_body_125_rms={ten_km_body:.6} ten_km_crack_4000_rms={ten_km_crack:.6}"
        );

        assert!(one_km_body > 0.45);
        assert!(ten_km_body > 0.20);
        assert!(ten_km_body < one_km_body);
        assert!(one_km_crack < one_km_body * 0.35);
        assert!(ten_km_crack < one_km_crack * 0.75);
    }

    #[test]
    fn each_weather_input_changes_tone_but_not_the_fixed_timing_speed() {
        let baseline = FrozenAtmosphere::freeze(Some(FALLBACK_ATMOSPHERE_OBSERVATION));
        for changed_observation in [
            AtmosphereObservation {
                temperature_c: 21.0,
                ..FALLBACK_ATMOSPHERE_OBSERVATION
            },
            AtmosphereObservation {
                relative_humidity_percent: 51.0,
                ..FALLBACK_ATMOSPHERE_OBSERVATION
            },
            AtmosphereObservation {
                pressure_kpa: 100.0,
                ..FALLBACK_ATMOSPHERE_OBSERVATION
            },
        ] {
            let changed = FrozenAtmosphere::freeze(Some(changed_observation));
            assert_eq!(changed.provenance(), AtmosphereProvenance::HostProvided);
            assert_ne!(
                baseline.absorption_db_per_meter().map(f64::to_bits),
                changed.absorption_db_per_meter().map(f64::to_bits)
            );
        }
        assert_eq!(
            fightbox_api::EngineConfig::default()
                .speed_of_sound_mps
                .to_bits(),
            343.0_f32.to_bits()
        );
    }
}
