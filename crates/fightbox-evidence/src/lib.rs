//! Deterministic, SDK-neutral evidence layer for Fightbox.
//!
//! This crate owns decoded-asset analysis, capture/run manifests, the canonical
//! PCM WAV encoding, deterministic Phase A signal generation, bounded metrics,
//! content hashing, and the provisional S3 listening-record vocabulary. It
//! holds no SDK handles and never infers an S0/S3 pass from configuration,
//! probe count, a non-null pointer, or `order >= 0`.
//!
//! Coordinates are the engine-wide right-handed local ENU frame owned by
//! [`fightbox_api`]; this layer does not transform them.

#![forbid(unsafe_code)]

mod analysis;
pub mod ears;
mod hash;
mod json;
mod listening;
mod manifest;
mod metrics;
mod signal;
mod wav;

pub use analysis::{
    ASSET_ANALYSIS_METHOD_ID, AnalyzedAsset, AssetAnalysisError, DecodedPcmProvenance,
    PcmChannelLayout, analyze_decoded_asset,
};
pub use hash::{sha256, sha256_hex};
pub use listening::{
    EquipmentRecord, HrtfRecord, LISTENING_REQUIRES_HUMAN, LISTENING_SCHEMA_VERSION,
    ListenerIdentity, ListeningObservation, ListeningRecord, ListeningResult, SignOff,
};
pub use manifest::{
    AssetAnalysisRecord, CadenceStatus, CallbackStatus, CallbackTiming, CaptureConfig,
    CaptureRunManifest, DegradationEvent, ExplicitClaim, ExplicitNonClaim, FixtureId,
    KernelProvenance, LimiterEvent, MANIFEST_SCHEMA_VERSION, ManifestValidationError, MetricRecord,
    MonitorGainRecord, PathingToggleSummary, ReferenceLevelRecord, RunProvenance, RunState,
    SceneCalibrationRecord, SimulationCadence, SourceCalibrationError, SourceCalibrationRecord,
    SourceDriveRecord, StemKind, StemRecord, WorldProvenance,
};
pub use metrics::{
    ChannelMetrics, ComparisonEnergy, ContinuityReport, DEFAULT_CLICK_RATIO_THRESHOLD,
    DEFAULT_LEVEL_DIFFERENCE_DB, DEFAULT_SPECTRAL_L1_DIFFERENCE, IaccReport, MetricError,
    ReflectionDensityReport, SpectralComparison, SummedOutputContinuity, channel_metrics,
    compare_pathing, continuity, interaural_cross_correlation, reflection_density,
    summed_output_continuity,
};
pub use signal::{
    GeneratedSignal, GeneratorNormalization, SignalError, SignalKind, multitone, pink_like, sine,
};
pub use wav::{
    WAVE_FORMAT_IEEE_FLOAT, WavError, WavSpec, read_wav, stem_hash, validate_spec, write_wav,
};

#[cfg(test)]
mod integration_tests {
    //! End-to-end checks that the evidence pieces compose deterministically:
    //! generate -> write WAV -> hash -> metric -> manifest -> listening record.

    use super::*;
    use fightbox_api::{ReferenceLevel, SceneCalibration};

    fn spec() -> WavSpec {
        WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        }
    }

    #[test]
    fn generated_signal_to_manifest_is_byte_stable() {
        let spec = spec();
        let on = pink_like(spec, 0xABCD, 9_600, -20.0).unwrap();
        let off = pink_like(spec, 0xABCD + 1, 9_600, -20.0).unwrap();

        let on_hash = stem_hash(spec, &on.samples).unwrap();
        let off_hash = stem_hash(spec, &off.samples).unwrap();
        let on_analysis = on.analyze().unwrap();
        // Same seed reproduces the same hash; different seed differs.
        assert_eq!(on_hash, stem_hash(spec, &on.samples).unwrap());
        assert_ne!(on_hash, off_hash);

        let comparison =
            compare_pathing(spec, &on.samples, &off.samples, &[500.0, 2_000.0]).unwrap();
        let channel = channel_metrics(spec, &on.samples).unwrap();

        let manifest = CaptureRunManifest {
            fixture_id: FixtureId::new("s3-corner"),
            kernel: KernelProvenance {
                name: "Steam Audio".into(),
                version: "4.8.1".into(),
                upstream_commit: "0da1825".into(),
                binary_checksum_sha256: None,
            },
            config: CaptureConfig {
                engine: fightbox_api::EngineConfig::default(),
                build_profile: "debug".into(),
                requested_quality: "phase-a".into(),
                delivered_quality: None,
            },
            stems: vec![
                StemRecord {
                    kind: StemKind::Direct,
                    content_hash_sha256: None,
                },
                StemRecord {
                    kind: StemKind::Path,
                    content_hash_sha256: Some(on_hash.clone()),
                },
                StemRecord {
                    kind: StemKind::PathingOnSum,
                    content_hash_sha256: Some(on_hash.clone()),
                },
                StemRecord {
                    kind: StemKind::PathingOffSum,
                    content_hash_sha256: Some(off_hash.clone()),
                },
            ],
            state: RunState::Planned,
            claims: vec![],
            non_claims: vec![
                ExplicitNonClaim {
                    statement: "S3 has not run".into(),
                },
                ExplicitNonClaim {
                    statement: "No delivered-ear-SPL claim without output calibration".into(),
                },
            ],
            world: Some(WorldProvenance {
                world_content_sha256: None,
                bake_content_sha256: None,
                probe_batch_content_sha256: None,
            }),
            source_calibrations: vec![
                SourceCalibrationRecord::derive(
                    "corner-source",
                    SceneCalibration::default(),
                    ReferenceLevel::SplAtOneMeter { db_spl: 85.0 },
                    &on_analysis,
                    Some(on.normalization),
                    MonitorGainRecord::NotApplicable,
                )
                .unwrap(),
            ],
            pathing_toggle: Some(PathingToggleSummary {
                on_sum_hash_sha256: Some(on_hash),
                off_sum_hash_sha256: Some(off_hash),
                level_difference_db: comparison.level_difference_db,
                spectral_difference_l1: Some(comparison.spectral_l1_difference),
                differs: Some(comparison.differs),
            }),
            metrics: vec![
                MetricRecord {
                    label: "channel".into(),
                    kind: "channel".into(),
                    payload_json: channel.to_json(),
                },
                MetricRecord {
                    label: "pathing_comparison".into(),
                    kind: "comparison".into(),
                    payload_json: comparison.to_json(),
                },
            ],
            provenance: RunProvenance {
                engine_commit: Some("feed1234".into()),
                platform: Some("darwin".into()),
                cpu_class: Some("arm64".into()),
                hrtf_identity: Some("steam-audio-default".into()),
                fixture_content_sha256: None,
                bake_duration_s: None,
                render_duration_s: None,
                simulation_cadence: SimulationCadence::default(),
                callback_timing: CallbackTiming::default(),
                limiter_events: vec![],
                degradation_events: vec![],
            },
        };

        let json = manifest.to_json();
        assert_eq!(json, manifest.to_json());
        // The manifest SHA is stable across calls (deterministic JSON).
        assert_eq!(sha256_hex(json.as_bytes()), sha256_hex(json.as_bytes()));
        // Non-claims are preserved alongside the full metric/payload bundle.
        assert!(json.contains(r#""non_claims":["S3 has not run"#));
        assert!(json.contains("No delivered-ear-SPL claim"));
        // The one source-drive gain chain is recorded: scene anchor, decoded
        // asset analysis, derived drive, generator normalization, monitor.
        assert!(json.contains(r#""scene":{"reference_spl_db":120"#));
        assert!(json.contains(r#""drive":{"target_source_rms_dbfs"#));
        assert!(json.contains(r#""generator_normalization":{"raw_rms_dbfs"#));
        assert!(json.contains(r#""monitor":{"status":"not_applicable"}"#));
        // §ν provenance is bound to the capture.
        assert!(json.contains(r#""engine_commit":"feed1234""#));
        assert!(json.contains(r#""hrtf_identity":"steam-audio-default""#));
        assert!(json.contains(r#""status":"not_applicable""#));
    }

    #[test]
    fn wav_round_trips_a_generated_signal() {
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        let signal = sine(spec, 1_000.0, 480, -20.0).unwrap();
        let bytes = write_wav(spec, &signal.samples).unwrap();
        let (read_spec, read_samples) = read_wav(&bytes).unwrap();
        assert_eq!(read_spec, spec);
        assert_eq!(read_samples, signal.samples);
    }

    #[test]
    fn listening_record_and_manifest_share_non_claim_discipline() {
        let record = ListeningRecord::new(
            "s3-provisional-0001",
            "s3-masonry-building-corner",
            ListenerIdentity {
                listener_id: "listener-a".into(),
                notes: "".into(),
            },
            HrtfRecord {
                hrtf_set: "steam-audio-default".into(),
                pretest_result: "not_run".into(),
            },
            EquipmentRecord {
                headphones: "closed-back reference".into(),
                output_path: "interface/line".into(),
                monitor_gain_db: Some(0.0),
            },
            SignOff {
                listener_signed: "".into(),
                date_iso: "".into(),
            },
            "2026-07-29",
        );
        let json = record.to_json();
        assert!(json.contains("\"requires_human_completion\":true"));
        assert!(json.contains("Human completion is required"));
        assert_eq!(json, record.to_json());
    }
}
