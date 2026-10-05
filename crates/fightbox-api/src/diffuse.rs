//! Pack-local shared diffuse-field profile.
//!
//! This is one listener-centric environmental tail. It deliberately carries no
//! per-source direction or discrete echo claim.

/// Authored controls for the V1 shared diffuse field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffuseFieldProfile {
    /// Linear wet gain after the shared field. Zero is a structural audible bypass.
    pub wet_gain: f32,
    /// Mid-band reverberation time to -60 dB.
    pub rt60_s: f32,
    /// Normalized high-frequency damping in the feedback paths.
    pub high_frequency_damping: f32,
}

impl DiffuseFieldProfile {
    pub const OFF: Self = Self {
        wet_gain: 0.0,
        rt60_s: 0.8,
        high_frequency_damping: 0.5,
    };

    /// Conservative authored interior used by focused fixtures.
    pub const SMALL_INTERIOR: Self = Self {
        wet_gain: 0.22,
        rt60_s: 1.1,
        high_frequency_damping: 0.58,
    };

    pub fn validate(self) -> Result<(), DiffuseFieldProfileError> {
        if !self.wet_gain.is_finite() || !(0.0..=1.0).contains(&self.wet_gain) {
            return Err(DiffuseFieldProfileError::InvalidWetGain);
        }
        if !self.rt60_s.is_finite() || !(0.1..=10.0).contains(&self.rt60_s) {
            return Err(DiffuseFieldProfileError::InvalidRt60);
        }
        if !self.high_frequency_damping.is_finite()
            || !(0.0..=1.0).contains(&self.high_frequency_damping)
        {
            return Err(DiffuseFieldProfileError::InvalidHighFrequencyDamping);
        }
        Ok(())
    }
}

impl Default for DiffuseFieldProfile {
    fn default() -> Self {
        Self::OFF
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffuseFieldProfileError {
    InvalidWetGain,
    InvalidRt60,
    InvalidHighFrequencyDamping,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_envelope_is_explicit_and_finite() {
        assert_eq!(DiffuseFieldProfile::OFF.validate(), Ok(()));
        assert_eq!(DiffuseFieldProfile::SMALL_INTERIOR.validate(), Ok(()));
        assert_eq!(
            DiffuseFieldProfile {
                wet_gain: 1.01,
                ..DiffuseFieldProfile::SMALL_INTERIOR
            }
            .validate(),
            Err(DiffuseFieldProfileError::InvalidWetGain)
        );
    }
}
