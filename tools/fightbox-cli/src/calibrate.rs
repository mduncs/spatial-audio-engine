//! Derives the one physical source drive (ADR 0002) from a fixture source and
//! the analyzed asset it plays.
//!
//! This routes the declared `SplAtOneMeter` SPL and the decoded program RMS
//! through [`SceneCalibration::derive_source_drive`]. It records the canonical
//! example (85 dB SPL + a -20 dBFS program derives a -59 dBFS target source RMS
//! and a -39 dB drive) so a capture is bound to the one gain chain and nothing
//! else. No delivered-ear SPL is claimed here.

use fightbox_api::{ReferenceLevel, SceneCalibration, SourceDrive};
use fightbox_evidence::AnalyzedAsset;

use crate::asset::ResolvedAsset;
use crate::fixture::Fixture;

/// The one source-drive chain bound to a fixture source.
#[derive(Clone, Copy, Debug)]
pub struct CalibratedSource {
    #[allow(dead_code)]
    pub reference_level: ReferenceLevel,
    pub drive: SourceDrive,
    /// Delivered program RMS in dBFS, recorded beside the drive.
    pub program_rms_dbfs: f32,
    /// Expected post-drive true peak in dBTP, recorded beside the drive.
    #[allow(dead_code)]
    pub expected_true_peak_dbtp: f32,
}

impl CalibratedSource {
    /// Derive the drive for a fixture source from the analyzed asset.
    #[allow(dead_code)]
    pub fn derive(fixture: &Fixture, asset: &ResolvedAsset) -> Result<Self, String> {
        let (_signal, analysis) = asset.regenerate_mono()?;
        Self::derive_from_analysis(fixture, &analysis)
    }

    /// Derive the drive from an already-analyzed asset (used when the caller
    /// needs the regenerated signal separately).
    pub fn derive_from_analysis(
        fixture: &Fixture,
        analysis: &AnalyzedAsset,
    ) -> Result<Self, String> {
        let scene = SceneCalibration::default();
        let db_spl = fixture.source.reference_level.db_spl as f32;
        if !db_spl.is_finite() {
            return Err("source db_spl must be finite".into());
        }
        let reference_level = ReferenceLevel::SplAtOneMeter { db_spl };
        let drive = scene
            .derive_source_drive(reference_level, analysis.analysis())
            .map_err(|e| format!("source-drive derivation failed: {e:?}"))?;
        Ok(Self {
            reference_level,
            drive,
            program_rms_dbfs: analysis.analysis().program_rms_dbfs,
            expected_true_peak_dbtp: drive.expected_true_peak_dbtp(),
        })
    }

    /// Assert and record the canonical ADR 0002 example for an 85 dB SPL source
    /// playing a -20 dBFS program: a -59 dBFS target and a -39 dB drive. Returns
    /// the recorded values so a caller can embed them in a metrics sidecar.
    ///
    /// The tolerance is loose enough to absorb the pink generator's tiny RMS
    /// error yet tight enough to catch a doubled or missing gain.
    pub fn assert_canonical_85db_minus_20(&self) -> Result<CanonicalDrive, String> {
        const TARGET_DBFS: f32 = -59.0;
        const DRIVE_DB: f32 = -39.0;
        const TOLERANCE_DB: f32 = 0.5;
        let target = self.drive.target_source_rms_dbfs();
        let drive_db = self.drive.gain_db();
        if (target - TARGET_DBFS).abs() > TOLERANCE_DB {
            return Err(format!(
                "85 dB SPL / -20 dBFS program must derive ~{TARGET_DBFS} dBFS target, got {target}"
            ));
        }
        if (drive_db - DRIVE_DB).abs() > TOLERANCE_DB {
            return Err(format!(
                "85 dB SPL / -20 dBFS program must derive ~{DRIVE_DB} dB drive, got {drive_db}"
            ));
        }
        Ok(CanonicalDrive {
            program_rms_dbfs: self.program_rms_dbfs,
            target_source_rms_dbfs: target,
            drive_gain_db: drive_db,
            linear_gain: self.drive.linear_gain(),
            tolerance_db: TOLERANCE_DB,
        })
    }
}

/// Snapshot of the one gain chain recorded for the canonical example.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct CanonicalDrive {
    pub program_rms_dbfs: f32,
    pub target_source_rms_dbfs: f32,
    pub drive_gain_db: f32,
    pub linear_gain: f32,
    pub tolerance_db: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AssetDescriptor, ResolvedAsset};
    use crate::fixture::test_fixtures;

    fn resolve_pink(text: &str) -> ResolvedAsset {
        AssetDescriptor::parse(text).unwrap().resolve().unwrap()
    }

    #[test]
    fn canonical_85db_minus_20_records_minus_59_and_minus_39() {
        let fixture = test_fixtures::s0();
        let asset = resolve_pink(include_str!(
            "../../../fixtures/assets/s0-calibrated-pink.json"
        ));
        let source = CalibratedSource::derive(&fixture, &asset).unwrap();
        let canonical = source.assert_canonical_85db_minus_20().unwrap();
        assert!((canonical.target_source_rms_dbfs - (-59.0)).abs() < 0.5);
        assert!((canonical.drive_gain_db - (-39.0)).abs() < 0.5);
        assert!(canonical.linear_gain > 0.0);
    }

    #[test]
    fn derivation_rejects_non_finite_db_spl() {
        let mut fixture = test_fixtures::s0();
        fixture.source.reference_level.db_spl = f64::NAN;
        let asset = resolve_pink(include_str!(
            "../../../fixtures/assets/s0-calibrated-pink.json"
        ));
        assert!(CalibratedSource::derive(&fixture, &asset).is_err());
    }
}
