//! Vendor-neutral domain types for Fightbox.
//!
//! Coordinates are right-handed local ENU: x east, y north, z up. No SDK handle
//! or SDK-specific coordinate type is allowed across this boundary.

#![forbid(unsafe_code)]

/// A position or direction in right-handed local ENU metres.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EnuVector3 {
    pub east_m: f32,
    pub north_m: f32,
    pub up_m: f32,
}

impl EnuVector3 {
    #[must_use]
    pub const fn new(east_m: f32, north_m: f32, up_m: f32) -> Self {
        Self {
            east_m,
            north_m,
            up_m,
        }
    }

    #[must_use]
    pub const fn is_finite(self) -> bool {
        self.east_m.is_finite() && self.north_m.is_finite() && self.up_m.is_finite()
    }
}

/// Position and orientation expressed in the engine's ENU frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub position: EnuVector3,
    pub forward: EnuVector3,
    pub up: EnuVector3,
}

/// A host-provided stable source identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub String);

impl SourceId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// The only supported distance for a scene SPL/PCM anchor.
pub const ONE_METER: f32 = 1.0;

/// The declared level at the source, before propagation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReferenceLevel {
    /// A relative gain applied to the decoded program.
    ///
    /// This is deterministic PCM gain. It makes no physical SPL, reach, or
    /// audibility-threshold claim.
    CreativeDb { db: f32 },
    /// A declared scene sound-pressure level at exactly one metre.
    SplAtOneMeter { db_spl: f32 },
}

impl ReferenceLevel {
    pub fn validate(self) -> Result<(), CalibrationError> {
        let db = match self {
            Self::CreativeDb { db } | Self::SplAtOneMeter { db_spl: db } => db,
        };
        if !db.is_finite() {
            return Err(CalibrationError::NonFiniteReferenceLevel);
        }
        Ok(())
    }
}

/// Scene-owned affine mapping between source SPL and source PCM RMS.
///
/// This is a digital scene calibration. It does not describe SPL at the ear
/// or the transfer function of an output device.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneCalibration {
    pub reference_spl_db: f32,
    pub reference_pcm_rms_dbfs: f32,
    pub reference_distance_m: f32,
}

impl SceneCalibration {
    pub const DEFAULT_REFERENCE_SPL_DB: f32 = 120.0;
    pub const DEFAULT_REFERENCE_PCM_RMS_DBFS: f32 = -24.0;
    pub const REFERENCE_DISTANCE_M: f32 = ONE_METER;

    pub fn validate(self) -> Result<(), CalibrationError> {
        if !self.reference_spl_db.is_finite() {
            return Err(CalibrationError::NonFiniteSceneReferenceSpl);
        }
        if !self.reference_pcm_rms_dbfs.is_finite() {
            return Err(CalibrationError::NonFiniteSceneReferencePcmRms);
        }
        if self.reference_distance_m != Self::REFERENCE_DISTANCE_M {
            return Err(CalibrationError::ReferenceDistanceMustBeOneMeter);
        }
        Ok(())
    }

    /// Maps a declared SPL at one metre to pre-propagation source PCM RMS.
    pub fn source_rms_dbfs_for_spl_at_one_meter(
        self,
        spl_db: f32,
    ) -> Result<f32, CalibrationError> {
        self.validate()?;
        if !spl_db.is_finite() {
            return Err(CalibrationError::NonFiniteReferenceLevel);
        }
        let source_rms_dbfs = self.reference_pcm_rms_dbfs + (spl_db - self.reference_spl_db);
        if !source_rms_dbfs.is_finite() {
            return Err(CalibrationError::NonFiniteDerivedLevel);
        }
        Ok(source_rms_dbfs)
    }

    /// Inverts the scene anchor for a source meter reading.
    ///
    /// The result is scene SPL at one metre, not delivered-ear SPL.
    pub fn spl_at_one_meter_for_source_rms_dbfs(
        self,
        source_rms_dbfs: f32,
    ) -> Result<f32, CalibrationError> {
        self.validate()?;
        if !source_rms_dbfs.is_finite() {
            return Err(CalibrationError::NonFiniteDerivedLevel);
        }
        let spl_db = self.reference_spl_db + (source_rms_dbfs - self.reference_pcm_rms_dbfs);
        if !spl_db.is_finite() {
            return Err(CalibrationError::NonFiniteDerivedLevel);
        }
        Ok(spl_db)
    }

    /// Derives the one source-drive gain. Callers do not provide another
    /// loudness gain alongside this result.
    pub fn derive_source_drive(
        self,
        level: ReferenceLevel,
        asset: &AssetAnalysis,
    ) -> Result<SourceDrive, CalibrationError> {
        self.validate()?;
        level.validate()?;
        asset.validate()?;

        let (target_source_rms_dbfs, spl_at_one_meter_db) = match level {
            ReferenceLevel::CreativeDb { db } => {
                let target = asset.program_rms_dbfs + db;
                (target, None)
            }
            ReferenceLevel::SplAtOneMeter { db_spl } => (
                self.source_rms_dbfs_for_spl_at_one_meter(db_spl)?,
                Some(db_spl),
            ),
        };
        if !target_source_rms_dbfs.is_finite() {
            return Err(CalibrationError::NonFiniteDerivedLevel);
        }

        let gain_db = target_source_rms_dbfs - asset.program_rms_dbfs;
        if !gain_db.is_finite() {
            return Err(CalibrationError::NonFiniteDerivedGain);
        }
        let linear_gain = 10.0_f32.powf(gain_db / 20.0);
        if !linear_gain.is_finite() || linear_gain <= 0.0 {
            return Err(CalibrationError::NonFiniteDerivedGain);
        }

        let expected_true_peak_dbtp = asset.true_peak_dbtp + gain_db;
        if !expected_true_peak_dbtp.is_finite() {
            return Err(CalibrationError::NonFiniteDerivedLevel);
        }

        Ok(SourceDrive {
            target_source_rms_dbfs,
            expected_true_peak_dbtp,
            gain_db,
            linear_gain,
            spl_at_one_meter_db,
        })
    }
}

impl Default for SceneCalibration {
    fn default() -> Self {
        Self {
            reference_spl_db: Self::DEFAULT_REFERENCE_SPL_DB,
            reference_pcm_rms_dbfs: Self::DEFAULT_REFERENCE_PCM_RMS_DBFS,
            reference_distance_m: Self::REFERENCE_DISTANCE_M,
        }
    }
}

/// Control-rate configuration for the digital output-safety chain.
///
/// The scene-SPL ceiling operates only on sources declared with
/// [`ReferenceLevel::SplAtOneMeter`]. It never claims an SPL at the output
/// device or listener's ear.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputSafetyConfig {
    pub scene_spl_ceiling_db: f32,
    pub scene_spl_knee_width_db: f32,
    pub default_source_radius_m: f32,
    pub monitor_gain_db: f32,
}

impl OutputSafetyConfig {
    // 130 dB engages only in genuine near-field danger (inside ~70 m of a
    // 155 dB source, ~2 m of a 118 dB siren). A city-scale ceiling like
    // 100 dB would sit below loud sources across the whole map and flatten
    // their distance gradient.
    pub const DEFAULT_SCENE_SPL_CEILING_DB: f32 = 130.0;
    pub const DEFAULT_SCENE_SPL_KNEE_WIDTH_DB: f32 = 12.0;
    pub const DEFAULT_SOURCE_RADIUS_M: f32 = ONE_METER;
    pub const DEFAULT_MONITOR_GAIN_DB: f32 = 0.0;

    pub fn validate(self) -> Result<(), OutputSafetyConfigError> {
        if !self.scene_spl_ceiling_db.is_finite() {
            return Err(OutputSafetyConfigError::InvalidSceneSplCeiling);
        }
        if !self.scene_spl_knee_width_db.is_finite() || self.scene_spl_knee_width_db <= 0.0 {
            return Err(OutputSafetyConfigError::InvalidSceneSplKneeWidth);
        }
        if !(self.scene_spl_ceiling_db - self.scene_spl_knee_width_db).is_finite() {
            return Err(OutputSafetyConfigError::InvalidSceneSplKneeWidth);
        }
        if !self.default_source_radius_m.is_finite() || self.default_source_radius_m <= 0.0 {
            return Err(OutputSafetyConfigError::InvalidSourceRadius);
        }
        let monitor_gain = 10.0_f32.powf(self.monitor_gain_db / 20.0);
        if !self.monitor_gain_db.is_finite() || !monitor_gain.is_finite() || monitor_gain <= 0.0 {
            return Err(OutputSafetyConfigError::InvalidMonitorGain);
        }
        Ok(())
    }
}

impl Default for OutputSafetyConfig {
    fn default() -> Self {
        Self {
            scene_spl_ceiling_db: Self::DEFAULT_SCENE_SPL_CEILING_DB,
            scene_spl_knee_width_db: Self::DEFAULT_SCENE_SPL_KNEE_WIDTH_DB,
            default_source_radius_m: Self::DEFAULT_SOURCE_RADIUS_M,
            monitor_gain_db: Self::DEFAULT_MONITOR_GAIN_DB,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSafetyConfigError {
    InvalidSceneSplCeiling,
    InvalidSceneSplKneeWidth,
    InvalidSourceRadius,
    InvalidMonitorGain,
    InvalidPosition,
    InvalidSourceIndex,
    InvalidSourceProfile,
    SourceNotConfigured,
}

/// Caller-owned identity for the method that produced an [`AssetAnalysis`].
///
/// The identifier must name the full-program RMS window/channel aggregation
/// rule and the true-peak method. It may also carry analyzer and version
/// identity. This crate validates supplied measurements; it does not perform
/// the analysis.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AssetMeasurementProvenance {
    pub method_id: String,
}

impl AssetMeasurementProvenance {
    pub fn new(method_id: impl Into<String>) -> Result<Self, CalibrationError> {
        let provenance = Self {
            method_id: method_id.into(),
        };
        provenance.validate()?;
        Ok(provenance)
    }

    pub fn validate(&self) -> Result<(), CalibrationError> {
        if self.method_id.trim().is_empty() {
            return Err(CalibrationError::MissingMeasurementProvenance);
        }
        Ok(())
    }
}

/// Caller-supplied measurements of decoded program PCM before source drive.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetAnalysis {
    pub program_rms_dbfs: f32,
    pub true_peak_dbtp: f32,
    pub measurement_provenance: AssetMeasurementProvenance,
}

impl AssetAnalysis {
    pub fn new(
        program_rms_dbfs: f32,
        true_peak_dbtp: f32,
        measurement_provenance: AssetMeasurementProvenance,
    ) -> Result<Self, CalibrationError> {
        let analysis = Self {
            program_rms_dbfs,
            true_peak_dbtp,
            measurement_provenance,
        };
        analysis.validate()?;
        Ok(analysis)
    }

    pub fn validate(&self) -> Result<(), CalibrationError> {
        self.measurement_provenance.validate()?;
        if self.program_rms_dbfs == f32::NEG_INFINITY {
            return Err(CalibrationError::SilentProgram);
        }
        if !self.program_rms_dbfs.is_finite() {
            return Err(CalibrationError::NonFiniteProgramRms);
        }
        if !self.true_peak_dbtp.is_finite() {
            return Err(CalibrationError::NonFiniteTruePeak);
        }
        if self.program_rms_dbfs > self.true_peak_dbtp {
            return Err(CalibrationError::ProgramRmsExceedsTruePeak);
        }
        Ok(())
    }
}

/// A validated source drive derived from scene calibration, source level, and
/// decoded asset analysis.
///
/// Its gain is applied to PCM exactly once before propagation branches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceDrive {
    target_source_rms_dbfs: f32,
    expected_true_peak_dbtp: f32,
    gain_db: f32,
    linear_gain: f32,
    spl_at_one_meter_db: Option<f32>,
}

impl SourceDrive {
    #[must_use]
    pub const fn target_source_rms_dbfs(self) -> f32 {
        self.target_source_rms_dbfs
    }

    /// The expected source RMS meter value after the one drive gain.
    #[must_use]
    pub const fn expected_source_meter_rms_dbfs(self) -> f32 {
        self.target_source_rms_dbfs
    }

    #[must_use]
    pub const fn expected_true_peak_dbtp(self) -> f32 {
        self.expected_true_peak_dbtp
    }

    #[must_use]
    pub const fn gain_db(self) -> f32 {
        self.gain_db
    }

    #[must_use]
    pub const fn linear_gain(self) -> f32 {
        self.linear_gain
    }

    /// Returns a free-field scene-SPL prediction when the source has a
    /// physical level, or `None` for `CreativeDb`.
    pub fn predicted_free_field_spl_db(
        self,
        distance_m: f32,
    ) -> Result<Option<f32>, CalibrationError> {
        let Some(spl_at_one_meter_db) = self.spl_at_one_meter_db else {
            return Ok(None);
        };
        let predicted = spl_at_one_meter_db + free_field_spreading_db(distance_m)?;
        if !predicted.is_finite() {
            return Err(CalibrationError::NonFiniteDerivedLevel);
        }
        Ok(Some(predicted))
    }
}

/// Free-field inverse-distance level delta relative to one metre.
pub fn free_field_spreading_db(distance_m: f32) -> Result<f32, CalibrationError> {
    if !distance_m.is_finite() || distance_m <= 0.0 {
        return Err(CalibrationError::InvalidPropagationDistance);
    }
    let delta_db = -20.0 * (distance_m / ONE_METER).log10();
    if !delta_db.is_finite() {
        return Err(CalibrationError::NonFiniteDerivedLevel);
    }
    Ok(delta_db)
}

/// Validation failures for the source calibration contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibrationError {
    NonFiniteSceneReferenceSpl,
    NonFiniteSceneReferencePcmRms,
    ReferenceDistanceMustBeOneMeter,
    MissingMeasurementProvenance,
    SilentProgram,
    NonFiniteProgramRms,
    NonFiniteTruePeak,
    ProgramRmsExceedsTruePeak,
    NonFiniteReferenceLevel,
    NonFiniteDerivedLevel,
    NonFiniteDerivedGain,
    InvalidPropagationDistance,
}

/// Spatial extent requested by a source.
///
/// The Steam Audio backend uses non-point extents to select a volumetric
/// direct-occlusion footprint. Spatial-width rendering is not implemented:
/// dry audio, pathing, and reflections are still rendered from one source
/// position.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ExtentDescriptor {
    #[default]
    Point,
    /// A compact point cloud. `count` must be nonzero.
    MultiPoint { count: u8 },
    /// A line whose length must be finite and positive.
    LineSegment { length_m: f32 },
    /// A stereo image whose width must be finite and positive.
    StereoImage { width_m: f32 },
}

impl ExtentDescriptor {
    /// Validates the finite, non-degenerate extent contract.
    pub fn validate(self) -> Result<(), ExtentError> {
        match self {
            Self::Point => Ok(()),
            Self::MultiPoint { count: 0 } => Err(ExtentError::EmptyMultiPoint),
            Self::MultiPoint { .. } => Ok(()),
            Self::LineSegment { length_m } if !length_m.is_finite() => {
                Err(ExtentError::NonFiniteLineLength)
            }
            Self::LineSegment { length_m } if length_m <= 0.0 => {
                Err(ExtentError::NonPositiveLineLength)
            }
            Self::LineSegment { .. } => Ok(()),
            Self::StereoImage { width_m } if !width_m.is_finite() => {
                Err(ExtentError::NonFiniteStereoWidth)
            }
            Self::StereoImage { width_m } if width_m <= 0.0 => {
                Err(ExtentError::NonPositiveStereoWidth)
            }
            Self::StereoImage { .. } => Ok(()),
        }
    }
}

/// Validation failures for a source extent descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtentError {
    EmptyMultiPoint,
    NonFiniteLineLength,
    NonPositiveLineLength,
    NonFiniteStereoWidth,
    NonPositiveStereoWidth,
}

/// Analytic source-radiation pattern matching Steam Audio's `IPLDirectivity` model.
///
/// `dipole_weight` blends an omnidirectional emitter (`0`) toward a pure dipole
/// (`1`), while `dipole_power` sharpens or broadens the resulting lobe. The
/// lobe is oriented by [`SourceProfile::pose`]'s forward vector; directivity
/// does not carry a second orientation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Directivity {
    /// Omni-to-dipole blend in the inclusive range
    /// [`Self::MIN_DIPOLE_WEIGHT`]..=[`Self::MAX_DIPOLE_WEIGHT`].
    pub dipole_weight: f32,
    /// Lobe exponent in the inclusive range
    /// [`Self::MIN_DIPOLE_POWER`]..=[`Self::MAX_DIPOLE_POWER`].
    ///
    /// The `0.25..=16` bound admits broad through sharply focused lobes while
    /// excluding non-positive exponents and impractically extreme values.
    pub dipole_power: f32,
}

impl Directivity {
    /// Exact legacy/default omnidirectional source model.
    pub const OMNIDIRECTIONAL: Self = Self {
        dipole_weight: 0.0,
        dipole_power: 1.0,
    };
    pub const MIN_DIPOLE_WEIGHT: f32 = 0.0;
    pub const MAX_DIPOLE_WEIGHT: f32 = 1.0;
    pub const MIN_DIPOLE_POWER: f32 = 0.25;
    pub const MAX_DIPOLE_POWER: f32 = 16.0;

    /// Validates the finite parameter ranges accepted by the source contract.
    pub fn validate(self) -> Result<(), DirectivityError> {
        if !self.dipole_weight.is_finite() {
            return Err(DirectivityError::NonFiniteDipoleWeight);
        }
        if !(Self::MIN_DIPOLE_WEIGHT..=Self::MAX_DIPOLE_WEIGHT).contains(&self.dipole_weight) {
            return Err(DirectivityError::InvalidDipoleWeight);
        }
        if !self.dipole_power.is_finite() {
            return Err(DirectivityError::NonFiniteDipolePower);
        }
        if !(Self::MIN_DIPOLE_POWER..=Self::MAX_DIPOLE_POWER).contains(&self.dipole_power) {
            return Err(DirectivityError::InvalidDipolePower);
        }
        Ok(())
    }
}

impl Default for Directivity {
    fn default() -> Self {
        Self::OMNIDIRECTIONAL
    }
}

/// Validation failures for a source directivity descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectivityError {
    NonFiniteDipoleWeight,
    InvalidDipoleWeight,
    NonFiniteDipolePower,
    InvalidDipolePower,
}

/// A complete vendor-neutral source declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceProfile {
    pub id: SourceId,
    pub pose: Pose,
    pub reference_level: ReferenceLevel,
    pub asset_analysis: AssetAnalysis,
    pub extent: ExtentDescriptor,
    /// Source-radiation lobe; [`Directivity::default`] preserves an omni source.
    pub directivity: Directivity,
    pub max_speed_mps: f32,
}

/// Listener state supplied to simulation workers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListenerState {
    pub pose: Pose,
    pub linear_velocity_mps: EnuVector3,
}

/// Configuration shared by the engine shell, independent of an acoustic SDK.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineConfig {
    pub sample_rate_hz: u32,
    pub block_size_frames: u32,
    pub speed_of_sound_mps: f32,
    pub max_active_sources: u8,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            block_size_frames: 128,
            speed_of_sound_mps: 343.0,
            max_active_sources: 8,
        }
    }
}

/// Domain validation errors. They intentionally say nothing about backend state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidSampleRate,
    InvalidBlockSize,
    InvalidSpeedOfSound,
    InvalidActiveSourceLimit,
}

impl EngineConfig {
    pub fn validate(self) -> Result<(), ConfigError> {
        if self.sample_rate_hz == 0 {
            return Err(ConfigError::InvalidSampleRate);
        }
        if self.block_size_frames == 0 {
            return Err(ConfigError::InvalidBlockSize);
        }
        if !self.speed_of_sound_mps.is_finite() || self.speed_of_sound_mps <= 0.0 {
            return Err(ConfigError::InvalidSpeedOfSound);
        }
        if self.max_active_sources == 0 {
            return Err(ConfigError::InvalidActiveSourceLimit);
        }
        Ok(())
    }
}

impl SourceProfile {
    /// Validation keeps calibrated SPL vocabulary explicit without inventing an output-ear SPL.
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.id.0.is_empty() {
            return Err(SourceError::EmptyId);
        }
        if !self.pose.position.is_finite()
            || !self.pose.forward.is_finite()
            || !self.pose.up.is_finite()
        {
            return Err(SourceError::NonFinitePose);
        }
        if !self.max_speed_mps.is_finite() || self.max_speed_mps < 0.0 {
            return Err(SourceError::InvalidMaxSpeed);
        }
        self.extent.validate().map_err(SourceError::Extent)?;
        self.directivity
            .validate()
            .map_err(SourceError::Directivity)?;
        self.asset_analysis
            .validate()
            .map_err(SourceError::Calibration)?;
        self.reference_level
            .validate()
            .map_err(SourceError::Calibration)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceError {
    EmptyId,
    NonFinitePose,
    InvalidMaxSpeed,
    Extent(ExtentError),
    Directivity(DirectivityError),
    Calibration(CalibrationError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1.0e-4,
            "expected {expected}, got {actual}"
        );
    }

    fn measured_asset(program_rms_dbfs: f32, true_peak_dbtp: f32) -> AssetAnalysis {
        AssetAnalysis::new(
            program_rms_dbfs,
            true_peak_dbtp,
            AssetMeasurementProvenance::new(
                "test/full-program-all-channel-rms+4x-oversampled-true-peak/v1",
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn source_profile_with_directivity(directivity: Directivity) -> SourceProfile {
        SourceProfile {
            id: SourceId::new("directivity-test"),
            pose: Pose {
                position: EnuVector3::default(),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            reference_level: ReferenceLevel::CreativeDb { db: 0.0 },
            asset_analysis: measured_asset(-20.0, -1.0),
            extent: ExtentDescriptor::Point,
            directivity,
            max_speed_mps: 0.0,
        }
    }

    #[test]
    fn default_directivity_is_exactly_omnidirectional() {
        let directivity = Directivity::default();
        assert_eq!(directivity.dipole_weight.to_bits(), 0.0_f32.to_bits());
        assert_eq!(directivity.dipole_power.to_bits(), 1.0_f32.to_bits());
        assert!(
            source_profile_with_directivity(directivity)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn source_profile_accepts_directivity_range_boundaries() {
        for directivity in [
            Directivity {
                dipole_weight: Directivity::MIN_DIPOLE_WEIGHT,
                dipole_power: Directivity::MIN_DIPOLE_POWER,
            },
            Directivity {
                dipole_weight: Directivity::MAX_DIPOLE_WEIGHT,
                dipole_power: Directivity::MAX_DIPOLE_POWER,
            },
            Directivity {
                dipole_weight: 0.7,
                dipole_power: 2.0,
            },
        ] {
            assert_eq!(
                source_profile_with_directivity(directivity).validate(),
                Ok(())
            );
        }
    }

    #[test]
    fn source_profile_rejects_nonfinite_and_out_of_range_directivity() {
        for (directivity, expected) in [
            (
                Directivity {
                    dipole_weight: f32::NAN,
                    dipole_power: 1.0,
                },
                DirectivityError::NonFiniteDipoleWeight,
            ),
            (
                Directivity {
                    dipole_weight: -f32::EPSILON,
                    dipole_power: 1.0,
                },
                DirectivityError::InvalidDipoleWeight,
            ),
            (
                Directivity {
                    dipole_weight: 1.0 + f32::EPSILON,
                    dipole_power: 1.0,
                },
                DirectivityError::InvalidDipoleWeight,
            ),
            (
                Directivity {
                    dipole_weight: 0.5,
                    dipole_power: f32::INFINITY,
                },
                DirectivityError::NonFiniteDipolePower,
            ),
            (
                Directivity {
                    dipole_weight: 0.5,
                    dipole_power: Directivity::MIN_DIPOLE_POWER - f32::EPSILON,
                },
                DirectivityError::InvalidDipolePower,
            ),
            (
                Directivity {
                    dipole_weight: 0.5,
                    dipole_power: Directivity::MAX_DIPOLE_POWER + 0.001,
                },
                DirectivityError::InvalidDipolePower,
            ),
        ] {
            assert_eq!(
                source_profile_with_directivity(directivity).validate(),
                Err(SourceError::Directivity(expected))
            );
        }
    }

    #[test]
    fn source_profile_validates_every_extent_variant() {
        for extent in [
            ExtentDescriptor::Point,
            ExtentDescriptor::MultiPoint { count: 1 },
            ExtentDescriptor::LineSegment { length_m: 0.25 },
            ExtentDescriptor::StereoImage { width_m: 4.0 },
        ] {
            let mut profile = source_profile_with_directivity(Directivity::default());
            profile.extent = extent;
            assert_eq!(profile.validate(), Ok(()), "{extent:?}");
        }

        for (extent, expected) in [
            (
                ExtentDescriptor::MultiPoint { count: 0 },
                ExtentError::EmptyMultiPoint,
            ),
            (
                ExtentDescriptor::LineSegment { length_m: 0.0 },
                ExtentError::NonPositiveLineLength,
            ),
            (
                ExtentDescriptor::LineSegment { length_m: -1.0 },
                ExtentError::NonPositiveLineLength,
            ),
            (
                ExtentDescriptor::LineSegment { length_m: f32::NAN },
                ExtentError::NonFiniteLineLength,
            ),
            (
                ExtentDescriptor::StereoImage { width_m: 0.0 },
                ExtentError::NonPositiveStereoWidth,
            ),
            (
                ExtentDescriptor::StereoImage { width_m: -1.0 },
                ExtentError::NonPositiveStereoWidth,
            ),
            (
                ExtentDescriptor::StereoImage {
                    width_m: f32::INFINITY,
                },
                ExtentError::NonFiniteStereoWidth,
            ),
        ] {
            let mut profile = source_profile_with_directivity(Directivity::default());
            profile.extent = extent;
            assert_eq!(
                profile.validate(),
                Err(SourceError::Extent(expected)),
                "{extent:?}"
            );
        }
    }

    #[test]
    fn default_config_is_the_phase_a_desktop_target() {
        assert_eq!(EngineConfig::default().sample_rate_hz, 48_000);
        assert_eq!(EngineConfig::default().block_size_frames, 128);
        assert!(EngineConfig::default().validate().is_ok());
    }

    #[test]
    fn spl_at_one_metre_is_distinct_from_creative_level() {
        assert_ne!(
            ReferenceLevel::CreativeDb { db: 85.0 },
            ReferenceLevel::SplAtOneMeter { db_spl: 85.0 }
        );
    }

    #[test]
    fn default_scene_anchor_maps_120_db_spl_to_minus_24_dbfs() {
        let calibration = SceneCalibration::default();

        assert_eq!(calibration.reference_spl_db, 120.0);
        assert_eq!(calibration.reference_pcm_rms_dbfs, -24.0);
        assert_eq!(calibration.reference_distance_m, ONE_METER);
        assert_eq!(
            calibration.source_rms_dbfs_for_spl_at_one_meter(120.0),
            Ok(-24.0)
        );
        assert_eq!(
            calibration.spl_at_one_meter_for_source_rms_dbfs(-24.0),
            Ok(120.0)
        );
    }

    #[test]
    fn eighty_five_db_source_derives_minus_59_target_and_minus_39_drive() {
        let asset = measured_asset(-20.0, -1.0);
        let drive = SceneCalibration::default()
            .derive_source_drive(ReferenceLevel::SplAtOneMeter { db_spl: 85.0 }, &asset)
            .unwrap();

        assert_eq!(drive.target_source_rms_dbfs(), -59.0);
        assert_eq!(drive.expected_source_meter_rms_dbfs(), -59.0);
        assert_eq!(drive.gain_db(), -39.0);
        assert_eq!(drive.expected_true_peak_dbtp(), -40.0);
        assert_close(drive.linear_gain(), 10.0_f32.powf(-39.0 / 20.0));
    }

    #[test]
    fn six_db_source_change_is_preserved_by_target_drive_and_meter() {
        let calibration = SceneCalibration::default();
        let asset = measured_asset(-20.0, -1.0);
        let base = calibration
            .derive_source_drive(ReferenceLevel::SplAtOneMeter { db_spl: 85.0 }, &asset)
            .unwrap();
        let raised = calibration
            .derive_source_drive(ReferenceLevel::SplAtOneMeter { db_spl: 91.0 }, &asset)
            .unwrap();

        assert_eq!(
            raised.target_source_rms_dbfs() - base.target_source_rms_dbfs(),
            6.0
        );
        assert_eq!(raised.gain_db() - base.gain_db(), 6.0);
        assert_eq!(
            raised.expected_source_meter_rms_dbfs() - base.expected_source_meter_rms_dbfs(),
            6.0
        );
    }

    #[test]
    fn inverse_distance_is_about_minus_40_db_at_100_metres() {
        assert_close(free_field_spreading_db(100.0).unwrap(), -40.0);

        let drive = SceneCalibration::default()
            .derive_source_drive(
                ReferenceLevel::SplAtOneMeter { db_spl: 85.0 },
                &measured_asset(-20.0, -1.0),
            )
            .unwrap();
        assert_close(
            drive.predicted_free_field_spl_db(100.0).unwrap().unwrap(),
            45.0,
        );
    }

    #[test]
    fn creative_db_is_relative_pcm_and_has_no_predicted_spl() {
        let drive = SceneCalibration::default()
            .derive_source_drive(
                ReferenceLevel::CreativeDb { db: 6.0 },
                &measured_asset(-20.0, -1.0),
            )
            .unwrap();

        assert_eq!(drive.target_source_rms_dbfs(), -14.0);
        assert_eq!(drive.gain_db(), 6.0);
        assert_eq!(drive.expected_source_meter_rms_dbfs(), -14.0);
        assert_eq!(drive.predicted_free_field_spl_db(100.0), Ok(None));
    }

    #[test]
    fn invalid_calibration_inputs_are_rejected() {
        assert_eq!(
            SceneCalibration {
                reference_spl_db: f32::NAN,
                ..SceneCalibration::default()
            }
            .validate(),
            Err(CalibrationError::NonFiniteSceneReferenceSpl)
        );
        assert_eq!(
            SceneCalibration {
                reference_pcm_rms_dbfs: f32::INFINITY,
                ..SceneCalibration::default()
            }
            .validate(),
            Err(CalibrationError::NonFiniteSceneReferencePcmRms)
        );
        assert_eq!(
            SceneCalibration {
                reference_distance_m: 0.999,
                ..SceneCalibration::default()
            }
            .validate(),
            Err(CalibrationError::ReferenceDistanceMustBeOneMeter)
        );
        assert_eq!(
            AssetAnalysis::new(
                f32::NEG_INFINITY,
                -1.0,
                AssetMeasurementProvenance::new("test-method").unwrap(),
            ),
            Err(CalibrationError::SilentProgram)
        );
        assert_eq!(
            AssetAnalysis::new(
                f32::NAN,
                -1.0,
                AssetMeasurementProvenance::new("test-method").unwrap(),
            ),
            Err(CalibrationError::NonFiniteProgramRms)
        );
        assert_eq!(
            AssetAnalysis::new(
                -20.0,
                f32::INFINITY,
                AssetMeasurementProvenance::new("test-method").unwrap(),
            ),
            Err(CalibrationError::NonFiniteTruePeak)
        );
        assert_eq!(
            AssetAnalysis::new(
                -1.0,
                -20.0,
                AssetMeasurementProvenance::new("test-method").unwrap(),
            ),
            Err(CalibrationError::ProgramRmsExceedsTruePeak)
        );
        assert_eq!(
            AssetMeasurementProvenance::new(" \t"),
            Err(CalibrationError::MissingMeasurementProvenance)
        );
        assert_eq!(
            ReferenceLevel::SplAtOneMeter { db_spl: f32::NAN }.validate(),
            Err(CalibrationError::NonFiniteReferenceLevel)
        );
        assert_eq!(
            SceneCalibration::default().derive_source_drive(
                ReferenceLevel::SplAtOneMeter { db_spl: f32::MAX },
                &measured_asset(-20.0, -1.0),
            ),
            Err(CalibrationError::NonFiniteDerivedGain)
        );
        assert_eq!(
            free_field_spreading_db(0.0),
            Err(CalibrationError::InvalidPropagationDistance)
        );
    }
}
