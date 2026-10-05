//! Frozen host observation for atmospheric sound absorption.
//!
//! The host obtains weather by whatever network or platform mechanism it
//! chooses, then supplies one observation when an acoustic session opens. The
//! engine never performs networking and never derives propagation timing from
//! this value.

/// Lowest temperature admitted by the ISO 9613-1 V1 observation envelope.
pub const MIN_ATMOSPHERE_TEMPERATURE_C: f32 = -20.0;
/// Highest temperature admitted by the ISO 9613-1 V1 observation envelope.
pub const MAX_ATMOSPHERE_TEMPERATURE_C: f32 = 50.0;
/// Lowest relative humidity admitted by the ISO 9613-1 V1 envelope.
pub const MIN_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT: f32 = 10.0;
/// Highest relative humidity admitted by the ISO 9613-1 V1 envelope.
pub const MAX_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT: f32 = 100.0;
/// Deterministic weather used whenever no valid host observation is present.
pub const FALLBACK_ATMOSPHERE_OBSERVATION: AtmosphereObservation = AtmosphereObservation {
    temperature_c: 20.0,
    relative_humidity_percent: 50.0,
    pressure_kpa: 101.325,
};

/// Host-provided atmospheric state frozen for one acoustic session.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtmosphereObservation {
    pub temperature_c: f32,
    pub relative_humidity_percent: f32,
    pub pressure_kpa: f32,
}

impl AtmosphereObservation {
    pub fn new(
        temperature_c: f32,
        relative_humidity_percent: f32,
        pressure_kpa: f32,
    ) -> Result<Self, AtmosphereObservationError> {
        let observation = Self {
            temperature_c,
            relative_humidity_percent,
            pressure_kpa,
        };
        observation.validate()?;
        Ok(observation)
    }

    pub fn validate(self) -> Result<(), AtmosphereObservationError> {
        if !self.temperature_c.is_finite() {
            return Err(AtmosphereObservationError::NonFiniteTemperature);
        }
        if !(MIN_ATMOSPHERE_TEMPERATURE_C..=MAX_ATMOSPHERE_TEMPERATURE_C)
            .contains(&self.temperature_c)
        {
            return Err(AtmosphereObservationError::TemperatureOutOfRange);
        }
        if !self.relative_humidity_percent.is_finite() {
            return Err(AtmosphereObservationError::NonFiniteRelativeHumidity);
        }
        if !(MIN_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT..=MAX_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT)
            .contains(&self.relative_humidity_percent)
        {
            return Err(AtmosphereObservationError::RelativeHumidityOutOfRange);
        }
        if !self.pressure_kpa.is_finite() {
            return Err(AtmosphereObservationError::NonFinitePressure);
        }
        if self.pressure_kpa <= 0.0 {
            return Err(AtmosphereObservationError::NonPositivePressure);
        }
        Ok(())
    }
}

/// Validation failure for a host atmosphere observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtmosphereObservationError {
    NonFiniteTemperature,
    TemperatureOutOfRange,
    NonFiniteRelativeHumidity,
    RelativeHumidityOutOfRange,
    NonFinitePressure,
    NonPositivePressure,
}

/// Why a frozen acoustic session uses its selected atmosphere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtmosphereProvenance {
    HostProvided,
    DeterministicFallbackMissing,
    DeterministicFallbackInvalid(AtmosphereObservationError),
}

impl AtmosphereProvenance {
    /// Stable text for manifests and evidence cards.
    #[must_use]
    pub const fn stable_label(self) -> &'static str {
        match self {
            Self::HostProvided => "host-provided",
            Self::DeterministicFallbackMissing => "deterministic-fallback/missing",
            Self::DeterministicFallbackInvalid(
                AtmosphereObservationError::NonFiniteTemperature,
            ) => "deterministic-fallback/nonfinite-temperature",
            Self::DeterministicFallbackInvalid(
                AtmosphereObservationError::TemperatureOutOfRange,
            ) => "deterministic-fallback/temperature-out-of-range",
            Self::DeterministicFallbackInvalid(
                AtmosphereObservationError::NonFiniteRelativeHumidity,
            ) => "deterministic-fallback/nonfinite-relative-humidity",
            Self::DeterministicFallbackInvalid(
                AtmosphereObservationError::RelativeHumidityOutOfRange,
            ) => "deterministic-fallback/relative-humidity-out-of-range",
            Self::DeterministicFallbackInvalid(AtmosphereObservationError::NonFinitePressure) => {
                "deterministic-fallback/nonfinite-pressure"
            }
            Self::DeterministicFallbackInvalid(AtmosphereObservationError::NonPositivePressure) => {
                "deterministic-fallback/nonpositive-pressure"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_fallback_is_the_frozen_plan_reference() {
        assert_eq!(FALLBACK_ATMOSPHERE_OBSERVATION.temperature_c, 20.0);
        assert_eq!(
            FALLBACK_ATMOSPHERE_OBSERVATION.relative_humidity_percent,
            50.0
        );
        assert_eq!(FALLBACK_ATMOSPHERE_OBSERVATION.pressure_kpa, 101.325);
        assert_eq!(FALLBACK_ATMOSPHERE_OBSERVATION.validate(), Ok(()));
    }

    #[test]
    fn documented_temperature_and_humidity_boundaries_are_valid() {
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
            assert_eq!(observation.validate(), Ok(()));
        }
    }

    #[test]
    fn every_invalid_field_has_a_stable_fallback_reason() {
        let cases = [
            (
                AtmosphereObservation {
                    temperature_c: f32::NAN,
                    ..FALLBACK_ATMOSPHERE_OBSERVATION
                },
                AtmosphereObservationError::NonFiniteTemperature,
                "deterministic-fallback/nonfinite-temperature",
            ),
            (
                AtmosphereObservation {
                    temperature_c: 50.1,
                    ..FALLBACK_ATMOSPHERE_OBSERVATION
                },
                AtmosphereObservationError::TemperatureOutOfRange,
                "deterministic-fallback/temperature-out-of-range",
            ),
            (
                AtmosphereObservation {
                    relative_humidity_percent: f32::INFINITY,
                    ..FALLBACK_ATMOSPHERE_OBSERVATION
                },
                AtmosphereObservationError::NonFiniteRelativeHumidity,
                "deterministic-fallback/nonfinite-relative-humidity",
            ),
            (
                AtmosphereObservation {
                    relative_humidity_percent: 9.9,
                    ..FALLBACK_ATMOSPHERE_OBSERVATION
                },
                AtmosphereObservationError::RelativeHumidityOutOfRange,
                "deterministic-fallback/relative-humidity-out-of-range",
            ),
            (
                AtmosphereObservation {
                    pressure_kpa: f32::NEG_INFINITY,
                    ..FALLBACK_ATMOSPHERE_OBSERVATION
                },
                AtmosphereObservationError::NonFinitePressure,
                "deterministic-fallback/nonfinite-pressure",
            ),
            (
                AtmosphereObservation {
                    pressure_kpa: 0.0,
                    ..FALLBACK_ATMOSPHERE_OBSERVATION
                },
                AtmosphereObservationError::NonPositivePressure,
                "deterministic-fallback/nonpositive-pressure",
            ),
        ];

        for (observation, expected_error, expected_label) in cases {
            assert_eq!(observation.validate(), Err(expected_error));
            assert_eq!(
                AtmosphereProvenance::DeterministicFallbackInvalid(expected_error).stable_label(),
                expected_label
            );
        }
        assert_eq!(
            AtmosphereProvenance::DeterministicFallbackMissing.stable_label(),
            "deterministic-fallback/missing"
        );
    }
}
