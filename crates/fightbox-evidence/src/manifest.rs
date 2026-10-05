//! Capture/run manifest structures with explicit evidence boundaries.
//!
//! The manifest is deterministic in caller-provided order. It records what a run
//! claims and what it explicitly does not claim; it never derives a gate pass
//! from fixture presence, configuration, probe count, or a non-null pointer.
//!
//! The Phase A extension below keeps the existing vocabulary and adds optional
//! fields for world/bake hashes, per-source calibration, the pathing-on/off
//! comparison, and typed metrics.

use fightbox_api::{
    AssetAnalysis, CalibrationError, EngineConfig, ReferenceLevel, SceneCalibration, SourceDrive,
};

use crate::analysis::{AnalyzedAsset, DecodedPcmProvenance, PcmChannelLayout};
use crate::json::{JsonObject, json_string_array};
use crate::signal::GeneratorNormalization;

pub const MANIFEST_SCHEMA_VERSION: &str = "fightbox.capture-run-manifest.v1";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FixtureId(pub String);
impl FixtureId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelProvenance {
    pub name: String,
    pub version: String,
    pub upstream_commit: String,
    pub binary_checksum_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaptureConfig {
    pub engine: EngineConfig,
    pub build_profile: String,
    pub requested_quality: String,
    pub delivered_quality: Option<String>,
}

/// Diagnostic stem kinds, including the pathing-on/off summed captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StemKind {
    Direct,
    Path,
    Reflections,
    PathingOnSum,
    PathingOffSum,
}
impl StemKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Path => "path",
            Self::Reflections => "reflections",
            Self::PathingOnSum => "pathing_on_sum",
            Self::PathingOffSum => "pathing_off_sum",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StemRecord {
    pub kind: StemKind,
    pub content_hash_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplicitClaim {
    pub statement: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplicitNonClaim {
    pub statement: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Planned,
    Completed,
    Failed,
}
impl RunState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Hashes for the world package, the baked probe data, and the serialized probe
/// batch. All are caller-supplied (the backend lane computes them) so this layer
/// stays SDK-neutral.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WorldProvenance {
    pub world_content_sha256: Option<String>,
    pub bake_content_sha256: Option<String>,
    pub probe_batch_content_sha256: Option<String>,
}

/// The scene-owned affine SPL-to-PCM anchor recorded for a source.
///
/// This mirrors the digital scene calibration of ADR 0002. It is a *digital
/// scene* anchor only — it does not describe SPL at the ear or the transfer of
/// an output device. By default it is the Fightbox anchor (120 dB SPL,
/// -24 dBFS RMS, 1 m).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneCalibrationRecord {
    pub reference_spl_db: f32,
    pub reference_pcm_rms_dbfs: f32,
    pub reference_distance_m: f32,
}

impl SceneCalibrationRecord {
    #[must_use]
    pub fn from_api(calibration: SceneCalibration) -> Self {
        Self {
            reference_spl_db: calibration.reference_spl_db,
            reference_pcm_rms_dbfs: calibration.reference_pcm_rms_dbfs,
            reference_distance_m: calibration.reference_distance_m,
        }
    }
}

impl Default for SceneCalibrationRecord {
    fn default() -> Self {
        Self::from_api(SceneCalibration::default())
    }
}

/// The decoded, pre-drive measurements of the asset program PCM recorded for a
/// source, including the exact decoded format that was analyzed.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetAnalysisRecord {
    pub program_rms_dbfs: f32,
    pub true_peak_dbtp: f32,
    /// Identifier naming the full-program RMS window/channel aggregation rule
    /// and the true-peak method, with analyzer/version identity where
    /// applicable.
    pub measurement_method_id: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub channel_layout: PcmChannelLayout,
    pub frame_count: usize,
}

impl AssetAnalysisRecord {
    #[must_use]
    pub fn from_analyzed_asset(analyzed: &AnalyzedAsset) -> Self {
        let analysis = analyzed.analysis();
        let pcm = analyzed.pcm();
        Self {
            program_rms_dbfs: analysis.program_rms_dbfs,
            true_peak_dbtp: analysis.true_peak_dbtp,
            measurement_method_id: analysis.measurement_provenance.method_id.clone(),
            sample_rate_hz: pcm.sample_rate_hz,
            channels: pcm.channels,
            channel_layout: pcm.channel_layout,
            frame_count: pcm.frame_count,
        }
    }

    fn to_api_analysis(&self) -> Result<AssetAnalysis, SourceCalibrationError> {
        let provenance =
            fightbox_api::AssetMeasurementProvenance::new(&self.measurement_method_id)?;
        Ok(AssetAnalysis::new(
            self.program_rms_dbfs,
            self.true_peak_dbtp,
            provenance,
        )?)
    }

    fn pcm_provenance(&self) -> DecodedPcmProvenance {
        DecodedPcmProvenance {
            sample_rate_hz: self.sample_rate_hz,
            channels: self.channels,
            channel_layout: self.channel_layout,
            frame_count: self.frame_count,
        }
    }
}

/// The derived source drive recorded for a source. Constructed only through
/// [`SceneCalibration::derive_source_drive`] (ADR 0002's one gain chain).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceDriveRecord {
    /// Target pre-propagation source RMS in dBFS after the one drive gain.
    pub target_source_rms_dbfs: f32,
    /// Expected post-drive true peak in dBTP for headroom reporting.
    pub expected_true_peak_dbtp: f32,
    /// The one drive gain in dB applied to PCM before propagation branches.
    pub drive_gain_db: f32,
    /// The drive gain as a linear multiplier (`10^(drive_gain_db / 20)`).
    pub linear_gain: f32,
}

impl SourceDriveRecord {
    #[must_use]
    pub fn from_drive(drive: SourceDrive) -> Self {
        Self {
            target_source_rms_dbfs: drive.target_source_rms_dbfs(),
            expected_true_peak_dbtp: drive.expected_true_peak_dbtp(),
            drive_gain_db: drive.gain_db(),
            linear_gain: drive.linear_gain(),
        }
    }
}

/// The reference level declared for a source, recorded as mode and value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReferenceLevelRecord {
    /// `"SplAtOneMeter"` or `"CreativeDb"`.
    pub mode: &'static str,
    /// The declared value (dB SPL for `SplAtOneMeter`, dB for `CreativeDb`).
    pub value_db: f32,
}

impl ReferenceLevelRecord {
    /// Records a [`ReferenceLevel`] as its mode/value pair.
    #[must_use]
    pub fn from_level(level: ReferenceLevel) -> Self {
        match level {
            ReferenceLevel::CreativeDb { db } => Self {
                mode: "CreativeDb",
                value_db: db,
            },
            ReferenceLevel::SplAtOneMeter { db_spl } => Self {
                mode: "SplAtOneMeter",
                value_db: db_spl,
            },
        }
    }
}

/// The monitor/output transfer recorded for a capture, kept explicitly distinct
/// from the scene calibration and source drive (ADR 0002). Monitor gain never
/// changes scene source power, propagation, or the source meter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MonitorGainRecord {
    /// A monitor gain in dB was applied to the rendered output. Downstream of,
    /// and separate from, the one source-drive gain chain.
    Applied { monitor_gain_db: f32 },
    /// No monitor gain or output transfer applies to this capture (e.g. an
    /// offline diagnostic render that drives no output device).
    NotApplicable,
}

/// One source's complete calibration chain as recorded into the manifest.
///
/// This records ADR 0002's single source-drive gain chain end to end:
/// scene anchor → decoded asset analysis → declared reference level → derived
/// source drive → (separately) generator normalization → (separately) monitor
/// gain/output transfer. There is no second caller-supplied loudness gain.
///
/// Use [`SourceCalibrationRecord::derive`] to build one from a validated
/// [`SceneCalibration`], [`ReferenceLevel`], and [`AnalyzedAsset`]; it routes
/// the drive through [`SceneCalibration::derive_source_drive`].
#[derive(Clone, Debug, PartialEq)]
pub struct SourceCalibrationRecord {
    source_id: String,
    scene: SceneCalibrationRecord,
    asset_analysis: AssetAnalysisRecord,
    reference_level: ReferenceLevelRecord,
    drive: SourceDriveRecord,
    /// The deterministic generator's normalization, recorded separately from the
    /// source drive when the source plays a generated asset. `None` for a source
    /// whose asset was not produced by this layer's generator.
    generator_normalization: Option<GeneratorNormalization>,
    /// Monitor gain / output transfer, explicitly distinct from the drive.
    monitor: MonitorGainRecord,
}

/// Error raised when a [`SourceCalibrationRecord`] cannot be derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceCalibrationError {
    /// The API's calibration contract rejected the inputs (non-finite levels,
    /// silent program, RMS exceeding true peak, etc.).
    Calibration(CalibrationError),
    InvalidPcmProvenance,
    InconsistentGeneratorNormalization,
    NonFiniteMonitorGain,
    InconsistentDerivedDrive,
}

impl From<CalibrationError> for SourceCalibrationError {
    fn from(error: CalibrationError) -> Self {
        Self::Calibration(error)
    }
}

impl SourceCalibrationRecord {
    /// Derives a record for a source from the one gain chain.
    ///
    /// The drive is produced by [`SceneCalibration::derive_source_drive`]; this
    /// constructor never supplies a second loudness gain. `generator_normalization`
    /// is recorded separately when the asset came from this layer's generator.
    pub fn derive(
        source_id: impl Into<String>,
        scene: SceneCalibration,
        level: ReferenceLevel,
        analyzed: &AnalyzedAsset,
        generator_normalization: Option<GeneratorNormalization>,
        monitor: MonitorGainRecord,
    ) -> Result<Self, SourceCalibrationError> {
        let asset = analyzed.analysis();
        validate_pcm_provenance(analyzed.pcm())?;
        validate_generator_normalization(generator_normalization, asset)?;
        if let MonitorGainRecord::Applied { monitor_gain_db } = monitor
            && !monitor_gain_db.is_finite()
        {
            return Err(SourceCalibrationError::NonFiniteMonitorGain);
        }
        let drive = scene.derive_source_drive(level, asset)?;
        let record = Self {
            source_id: source_id.into(),
            scene: SceneCalibrationRecord::from_api(scene),
            asset_analysis: AssetAnalysisRecord::from_analyzed_asset(analyzed),
            reference_level: ReferenceLevelRecord::from_level(level),
            drive: SourceDriveRecord::from_drive(drive),
            generator_normalization,
            monitor,
        };
        record.validate()?;
        Ok(record)
    }

    /// Recompute the one gain chain and reject any internal mismatch.
    ///
    /// Fields are private so a completed manifest can only receive a record
    /// made by [`Self::derive`]. This check remains public for import or storage
    /// boundaries that want to revalidate an in-memory record.
    pub fn validate(&self) -> Result<(), SourceCalibrationError> {
        let scene = SceneCalibration {
            reference_spl_db: self.scene.reference_spl_db,
            reference_pcm_rms_dbfs: self.scene.reference_pcm_rms_dbfs,
            reference_distance_m: self.scene.reference_distance_m,
        };
        let asset = self.asset_analysis.to_api_analysis()?;
        let pcm = self.asset_analysis.pcm_provenance();
        validate_pcm_provenance(pcm)?;
        validate_generator_normalization(self.generator_normalization, &asset)?;
        if let MonitorGainRecord::Applied { monitor_gain_db } = self.monitor
            && !monitor_gain_db.is_finite()
        {
            return Err(SourceCalibrationError::NonFiniteMonitorGain);
        }

        let level = match self.reference_level.mode {
            "CreativeDb" => ReferenceLevel::CreativeDb {
                db: self.reference_level.value_db,
            },
            "SplAtOneMeter" => ReferenceLevel::SplAtOneMeter {
                db_spl: self.reference_level.value_db,
            },
            _ => return Err(SourceCalibrationError::InconsistentDerivedDrive),
        };
        let recomputed = SourceDriveRecord::from_drive(scene.derive_source_drive(level, &asset)?);
        if !drive_records_match(self.drive, recomputed) {
            return Err(SourceCalibrationError::InconsistentDerivedDrive);
        }
        Ok(())
    }

    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    #[must_use]
    pub const fn asset_analysis(&self) -> &AssetAnalysisRecord {
        &self.asset_analysis
    }

    #[must_use]
    pub const fn drive(&self) -> SourceDriveRecord {
        self.drive
    }

    #[must_use]
    pub const fn reference_level(&self) -> ReferenceLevelRecord {
        self.reference_level
    }

    #[must_use]
    pub const fn generator_normalization(&self) -> Option<GeneratorNormalization> {
        self.generator_normalization
    }

    #[must_use]
    pub const fn monitor(&self) -> MonitorGainRecord {
        self.monitor
    }
}

fn validate_pcm_provenance(pcm: DecodedPcmProvenance) -> Result<(), SourceCalibrationError> {
    let expected_layout = match pcm.channels {
        1 => PcmChannelLayout::Mono,
        2 => PcmChannelLayout::StereoLeftRight,
        _ => return Err(SourceCalibrationError::InvalidPcmProvenance),
    };
    if pcm.sample_rate_hz == 0 || pcm.frame_count == 0 || pcm.channel_layout != expected_layout {
        return Err(SourceCalibrationError::InvalidPcmProvenance);
    }
    Ok(())
}

fn validate_generator_normalization(
    normalization: Option<GeneratorNormalization>,
    asset: &AssetAnalysis,
) -> Result<(), SourceCalibrationError> {
    let Some(normalization) = normalization else {
        return Ok(());
    };
    let values = [
        normalization.raw_rms_dbfs,
        normalization.target_rms_dbfs,
        normalization.normalization_gain_db,
    ];
    let target_from_gain = normalization.raw_rms_dbfs + normalization.normalization_gain_db;
    if values.iter().any(|value| !value.is_finite())
        || (target_from_gain - normalization.target_rms_dbfs).abs() > 1e-3
        || (normalization.target_rms_dbfs - asset.program_rms_dbfs).abs() > 1e-3
    {
        return Err(SourceCalibrationError::InconsistentGeneratorNormalization);
    }
    Ok(())
}

fn drive_records_match(left: SourceDriveRecord, right: SourceDriveRecord) -> bool {
    let close = |a: f32, b: f32| (a - b).abs() <= 1e-6;
    close(left.target_source_rms_dbfs, right.target_source_rms_dbfs)
        && close(left.expected_true_peak_dbtp, right.expected_true_peak_dbtp)
        && close(left.drive_gain_db, right.drive_gain_db)
        && close(left.linear_gain, right.linear_gain)
}

/// Result of the pathing-on vs pathing-off comparison.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PathingToggleSummary {
    pub on_sum_hash_sha256: Option<String>,
    pub off_sum_hash_sha256: Option<String>,
    pub level_difference_db: Option<f32>,
    pub spectral_difference_l1: Option<f32>,
    pub differs: Option<bool>,
}

/// Freshness of the active simulation snapshot. Offline Phase A captures that do
/// not stream a live simulation report [`CadenceStatus::NotApplicable`] rather
/// than implying a realtime cadence was met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CadenceStatus {
    /// The snapshot was up to date at capture time.
    Fresh,
    /// The snapshot was older than the configured cadence budget.
    Stale,
    /// The capture did not stream a live simulation (offline Phase A).
    NotApplicable,
}
impl CadenceStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// Simulation update cadence and snapshot age. Defaults to not-applicable so an
/// offline capture is honest about not exercising a streaming cadence.
#[derive(Clone, Debug, PartialEq)]
pub struct SimulationCadence {
    /// Frames between simulation updates (1 = every block).
    pub simulation_stride_frames: Option<u32>,
    /// Age of the active snapshot in frames at capture time.
    pub snapshot_age_frames: Option<u32>,
    pub status: CadenceStatus,
}
impl Default for SimulationCadence {
    fn default() -> Self {
        Self {
            simulation_stride_frames: None,
            snapshot_age_frames: None,
            status: CadenceStatus::NotApplicable,
        }
    }
}

/// Realtime callback deadline status. `NotApplicable` for an offline render that
/// never ran on the audio thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackStatus {
    /// Every callback met its deadline.
    Met,
    /// At least one callback overran its deadline.
    Faulted,
    /// The render was offline and never used the audio callback.
    NotApplicable,
}
impl CallbackStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::Faulted => "faulted",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// Realtime callback timing summary for a capture.
#[derive(Clone, Debug, PartialEq)]
pub struct CallbackTiming {
    pub status: CallbackStatus,
    pub deadline_fault_count: u32,
    /// Largest single over-run past the callback deadline, in seconds.
    pub max_callback_overrun_s: Option<f32>,
}
impl Default for CallbackTiming {
    fn default() -> Self {
        Self {
            status: CallbackStatus::NotApplicable,
            deadline_fault_count: 0,
            max_callback_overrun_s: None,
        }
    }
}

/// A limiter engagement event observed during the render.
#[derive(Clone, Debug, PartialEq)]
pub struct LimiterEvent {
    pub label: String,
    pub gain_reduction_db: Option<f32>,
    pub sample_frame: Option<u64>,
}

/// A degradation event (e.g. source drop, quality fallback) observed during the
/// render.
#[derive(Clone, Debug, PartialEq)]
pub struct DegradationEvent {
    pub label: String,
    pub detail: String,
    pub sample_frame: Option<u64>,
}

/// Phase A run provenance: engine build, host identity, phase durations, and
/// streaming health (authority note §ν).
///
/// Inner fields are optional so a *planned* manifest can be built before the run
/// executes; a *completed* manifest is expected to populate them. The runtime
/// health fields default to not-applicable so an offline capture never implies a
/// realtime cadence or callback budget it did not exercise.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunProvenance {
    /// Engine shell git commit that produced the capture (distinct from the
    /// kernel `upstream_commit`).
    pub engine_commit: Option<String>,
    pub platform: Option<String>,
    pub cpu_class: Option<String>,
    /// HRTF set identity used for the binaural render.
    pub hrtf_identity: Option<String>,
    /// SHA-256 over the fixture descriptor that parameterized the run.
    pub fixture_content_sha256: Option<String>,
    pub bake_duration_s: Option<f32>,
    pub render_duration_s: Option<f32>,
    pub simulation_cadence: SimulationCadence,
    pub callback_timing: CallbackTiming,
    pub limiter_events: Vec<LimiterEvent>,
    pub degradation_events: Vec<DegradationEvent>,
}

/// A typed metric embedded in the manifest. `payload_json` is the deterministic
/// JSON object produced by a metric's own `to_json()` (channel/comparison/...).
#[derive(Clone, Debug, PartialEq)]
pub struct MetricRecord {
    pub label: String,
    pub kind: String,
    pub payload_json: String,
}

/// A capture manifest contains a source chain that no longer recomputes to the
/// measurements and declaration recorded beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestValidationError {
    pub source_index: usize,
    pub error: SourceCalibrationError,
}

/// Canonical sidecar for a capture or attempted run.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureRunManifest {
    pub fixture_id: FixtureId,
    pub kernel: KernelProvenance,
    pub config: CaptureConfig,
    pub stems: Vec<StemRecord>,
    pub state: RunState,
    pub claims: Vec<ExplicitClaim>,
    pub non_claims: Vec<ExplicitNonClaim>,
    // Phase A extensions; all optional so the original vocabulary still composes.
    pub world: Option<WorldProvenance>,
    pub source_calibrations: Vec<SourceCalibrationRecord>,
    pub pathing_toggle: Option<PathingToggleSummary>,
    pub metrics: Vec<MetricRecord>,
    /// Authority-note §ν provenance: engine build, host, durations, runtime
    /// health. Always emitted so a consumer can bind a capture to its run.
    pub provenance: RunProvenance,
}

impl CaptureRunManifest {
    /// Recompute every source's scene/analysis/drive chain.
    ///
    /// This is required for completed records and also runs for planned/failed
    /// records so a state transition cannot make stale source math valid.
    pub fn validate(&self) -> Result<(), ManifestValidationError> {
        for (source_index, source) in self.source_calibrations.iter().enumerate() {
            source.validate().map_err(|error| ManifestValidationError {
                source_index,
                error,
            })?;
        }
        Ok(())
    }

    /// Serializes stable JSON without adding a serialization framework.
    ///
    /// Panics only if crate-internal corruption bypassed
    /// [`SourceCalibrationRecord::derive`]. Public callers cannot mutate the
    /// private source-chain fields.
    #[must_use]
    pub fn to_json(&self) -> String {
        self.validate()
            .expect("source calibration record must remain internally consistent");
        self.to_json_unchecked()
    }

    fn to_json_unchecked(&self) -> String {
        let mut o = JsonObject::new();
        o.str("schema_version", MANIFEST_SCHEMA_VERSION);
        o.str("fixture_id", &self.fixture_id.0);
        o.str("state", self.state.as_str());

        // kernel
        let mut kernel = JsonObject::new();
        kernel.str("name", &self.kernel.name);
        kernel.str("version", &self.kernel.version);
        kernel.str("upstream_commit", &self.kernel.upstream_commit);
        kernel.opt_str(
            "binary_checksum_sha256",
            self.kernel.binary_checksum_sha256.as_deref(),
        );
        o.raw_value("kernel", &kernel.finish());

        // config
        let mut config = JsonObject::new();
        config.num_u32("sample_rate_hz", self.config.engine.sample_rate_hz);
        config.num_u32("block_size_frames", self.config.engine.block_size_frames);
        config.num_f32("speed_of_sound_mps", self.config.engine.speed_of_sound_mps);
        config.num_u32(
            "max_active_sources",
            u32::from(self.config.engine.max_active_sources),
        );
        config.str("build_profile", &self.config.build_profile);
        config.str("requested_quality", &self.config.requested_quality);
        config.opt_str(
            "delivered_quality",
            self.config.delivered_quality.as_deref(),
        );
        o.raw_value("config", &config.finish());

        o.raw_value("stems", &self.stems_json());
        if let Some(world) = &self.world {
            o.raw_value("world", &world_json(world));
        }
        if !self.source_calibrations.is_empty() {
            o.raw_value(
                "source_calibrations",
                &calibrations_json(&self.source_calibrations),
            );
        }
        if let Some(toggle) = &self.pathing_toggle {
            o.raw_value("pathing_toggle", &toggle_json(toggle));
        }
        if !self.metrics.is_empty() {
            o.raw_value("metrics", &metrics_json(&self.metrics));
        }
        // Provenance is always emitted (authority note §ν): it binds the capture
        // to the engine build, host, durations, and runtime health.
        o.raw_value("provenance", &provenance_json(&self.provenance));

        let claims = json_string_array(self.claims.iter().map(|c| c.statement.as_str()));
        let non_claims = json_string_array(self.non_claims.iter().map(|c| c.statement.as_str()));
        o.raw_value("claims", &claims);
        o.raw_value("non_claims", &non_claims);
        o.finish()
    }

    fn stems_json(&self) -> String {
        let mut out = String::from("[");
        for (i, stem) in self.stems.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let mut s = JsonObject::new();
            s.str("kind", stem.kind.as_str());
            s.opt_str("content_hash_sha256", stem.content_hash_sha256.as_deref());
            out.push_str(&s.finish());
        }
        out.push(']');
        out
    }
}

fn world_json(world: &WorldProvenance) -> String {
    let mut o = JsonObject::new();
    o.opt_str(
        "world_content_sha256",
        world.world_content_sha256.as_deref(),
    );
    o.opt_str("bake_content_sha256", world.bake_content_sha256.as_deref());
    o.opt_str(
        "probe_batch_content_sha256",
        world.probe_batch_content_sha256.as_deref(),
    );
    o.finish()
}

fn calibrations_json(records: &[SourceCalibrationRecord]) -> String {
    let mut out = String::from("[");
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let mut o = JsonObject::new();
        o.str("source_id", &record.source_id);

        // scene anchor (digital scene calibration; ADR 0002)
        let mut scene = JsonObject::new();
        scene.num_f32("reference_spl_db", record.scene.reference_spl_db);
        scene.num_f32(
            "reference_pcm_rms_dbfs",
            record.scene.reference_pcm_rms_dbfs,
        );
        scene.num_f32("reference_distance_m", record.scene.reference_distance_m);
        o.raw_value("scene", &scene.finish());

        // decoded, pre-drive asset analysis
        let mut analysis = JsonObject::new();
        analysis.num_f32("program_rms_dbfs", record.asset_analysis.program_rms_dbfs);
        analysis.num_f32("true_peak_dbtp", record.asset_analysis.true_peak_dbtp);
        analysis.str(
            "measurement_method_id",
            &record.asset_analysis.measurement_method_id,
        );
        analysis.num_u32("sample_rate_hz", record.asset_analysis.sample_rate_hz);
        analysis.num_u32("channels", u32::from(record.asset_analysis.channels));
        analysis.str(
            "channel_layout",
            record.asset_analysis.channel_layout.as_str(),
        );
        analysis.num_usize("frame_count", record.asset_analysis.frame_count);
        o.raw_value("asset_analysis", &analysis.finish());

        // declared reference level
        let mut level = JsonObject::new();
        level.str("mode", record.reference_level.mode);
        level.num_f32("value_db", record.reference_level.value_db);
        o.raw_value("reference_level", &level.finish());

        // derived source drive (the one gain chain)
        let mut drive = JsonObject::new();
        drive.num_f32(
            "target_source_rms_dbfs",
            record.drive.target_source_rms_dbfs,
        );
        drive.num_f32(
            "expected_true_peak_dbtp",
            record.drive.expected_true_peak_dbtp,
        );
        drive.num_f32("drive_gain_db", record.drive.drive_gain_db);
        drive.num_f32("linear_gain", record.drive.linear_gain);
        o.raw_value("drive", &drive.finish());

        // generator normalization, recorded separately when applicable
        match &record.generator_normalization {
            Some(norm) => {
                let mut g = JsonObject::new();
                g.num_f32("raw_rms_dbfs", norm.raw_rms_dbfs);
                g.num_f32("target_rms_dbfs", norm.target_rms_dbfs);
                g.num_f32("normalization_gain_db", norm.normalization_gain_db);
                o.raw_value("generator_normalization", &g.finish());
            }
            None => o.raw_value("generator_normalization", "null"),
        }

        // monitor gain / output transfer, explicitly distinct from the drive
        match record.monitor {
            MonitorGainRecord::Applied { monitor_gain_db } => {
                let mut m = JsonObject::new();
                m.str("status", "applied");
                m.num_f32("monitor_gain_db", monitor_gain_db);
                o.raw_value("monitor", &m.finish());
            }
            MonitorGainRecord::NotApplicable => {
                let mut m = JsonObject::new();
                m.str("status", "not_applicable");
                o.raw_value("monitor", &m.finish());
            }
        }

        out.push_str(&o.finish());
    }
    out.push(']');
    out
}

fn toggle_json(toggle: &PathingToggleSummary) -> String {
    let mut o = JsonObject::new();
    o.opt_str("on_sum_hash_sha256", toggle.on_sum_hash_sha256.as_deref());
    o.opt_str("off_sum_hash_sha256", toggle.off_sum_hash_sha256.as_deref());
    o.opt_f32("level_difference_db", toggle.level_difference_db);
    o.opt_f32("spectral_difference_l1", toggle.spectral_difference_l1);
    match toggle.differs {
        Some(v) => o.boolean("differs", v),
        None => o.raw_value("differs", "null"),
    }
    o.finish()
}

fn metrics_json(records: &[MetricRecord]) -> String {
    let mut out = String::from("[");
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let mut o = JsonObject::new();
        o.str("label", &record.label);
        o.str("kind", &record.kind);
        o.raw_value("payload", &record.payload_json);
        out.push_str(&o.finish());
    }
    out.push(']');
    out
}

fn provenance_json(provenance: &RunProvenance) -> String {
    let mut o = JsonObject::new();
    o.opt_str("engine_commit", provenance.engine_commit.as_deref());
    o.opt_str("platform", provenance.platform.as_deref());
    o.opt_str("cpu_class", provenance.cpu_class.as_deref());
    o.opt_str("hrtf_identity", provenance.hrtf_identity.as_deref());
    o.opt_str(
        "fixture_content_sha256",
        provenance.fixture_content_sha256.as_deref(),
    );
    o.opt_f32("bake_duration_s", provenance.bake_duration_s);
    o.opt_f32("render_duration_s", provenance.render_duration_s);

    let mut cadence = JsonObject::new();
    opt_u32(
        &mut cadence,
        "simulation_stride_frames",
        provenance.simulation_cadence.simulation_stride_frames,
    );
    opt_u32(
        &mut cadence,
        "snapshot_age_frames",
        provenance.simulation_cadence.snapshot_age_frames,
    );
    cadence.str("status", provenance.simulation_cadence.status.as_str());
    o.raw_value("simulation_cadence", &cadence.finish());

    let mut callback = JsonObject::new();
    callback.str("status", provenance.callback_timing.status.as_str());
    callback.num_u32(
        "deadline_fault_count",
        provenance.callback_timing.deadline_fault_count,
    );
    callback.opt_f32(
        "max_callback_overrun_s",
        provenance.callback_timing.max_callback_overrun_s,
    );
    o.raw_value("callback_timing", &callback.finish());

    o.raw_value(
        "limiter_events",
        &limiter_events_json(&provenance.limiter_events),
    );
    o.raw_value(
        "degradation_events",
        &degradation_events_json(&provenance.degradation_events),
    );
    o.finish()
}

fn limiter_events_json(events: &[LimiterEvent]) -> String {
    let mut out = String::from("[");
    for (i, event) in events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let mut o = JsonObject::new();
        o.str("label", &event.label);
        o.opt_f32("gain_reduction_db", event.gain_reduction_db);
        opt_u64(&mut o, "sample_frame", event.sample_frame);
        out.push_str(&o.finish());
    }
    out.push(']');
    out
}

fn degradation_events_json(events: &[DegradationEvent]) -> String {
    let mut out = String::from("[");
    for (i, event) in events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let mut o = JsonObject::new();
        o.str("label", &event.label);
        o.str("detail", &event.detail);
        opt_u64(&mut o, "sample_frame", event.sample_frame);
        out.push_str(&o.finish());
    }
    out.push(']');
    out
}

/// Emit an optional `u64` as a JSON number or `null`.
fn opt_u64(o: &mut JsonObject, key: &str, value: Option<u64>) {
    match value {
        Some(v) => o.num_u64(key, v),
        None => o.raw_value(key, "null"),
    }
}

/// Emit an optional `u32` as a JSON number or `null`.
fn opt_u32(o: &mut JsonObject, key: &str, value: Option<u32>) {
    match value {
        Some(v) => o.num_u32(key, v),
        None => o.raw_value(key, "null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ASSET_ANALYSIS_METHOD_ID, WavSpec, sine};
    use fightbox_api::SceneCalibration;

    fn analyzed_minus_20_dbfs() -> AnalyzedAsset {
        sine(
            WavSpec {
                sample_rate_hz: 48_000,
                channels: 1,
            },
            1_000.0,
            48_000,
            -20.0,
        )
        .unwrap()
        .analyze()
        .unwrap()
    }

    /// An 85 dB SPL source playing a `-20 dBFS RMS` asset derives the ADR 0002
    /// canonical example: a `-59 dBFS` target source RMS and a `-39 dB` drive.
    fn canonical_85db_calibration() -> SourceCalibrationRecord {
        let analyzed = analyzed_minus_20_dbfs();
        SourceCalibrationRecord::derive(
            "corner-source",
            SceneCalibration::default(),
            ReferenceLevel::SplAtOneMeter { db_spl: 85.0 },
            &analyzed,
            None,
            MonitorGainRecord::NotApplicable,
        )
        .unwrap()
    }

    fn minimal_manifest() -> CaptureRunManifest {
        CaptureRunManifest {
            fixture_id: FixtureId::new("s3-corner"),
            kernel: KernelProvenance {
                name: "Steam Audio".into(),
                version: "4.8.1".into(),
                upstream_commit: "0da1825".into(),
                binary_checksum_sha256: None,
            },
            config: CaptureConfig {
                engine: EngineConfig::default(),
                build_profile: "debug".into(),
                requested_quality: "phase-a".into(),
                delivered_quality: None,
            },
            stems: vec![StemRecord {
                kind: StemKind::Path,
                content_hash_sha256: None,
            }],
            state: RunState::Planned,
            claims: vec![],
            non_claims: vec![ExplicitNonClaim {
                statement: "S3 has not run".into(),
            }],
            world: None,
            source_calibrations: vec![],
            pathing_toggle: None,
            metrics: vec![],
            provenance: RunProvenance::default(),
        }
    }

    #[test]
    fn manifest_is_deterministic_and_keeps_non_claims() {
        let manifest = minimal_manifest();
        let first = manifest.to_json();
        assert_eq!(first, manifest.to_json());
        assert!(first.contains(r#""non_claims":["S3 has not run"]"#));
        assert!(first.contains(r#""content_hash_sha256":null"#));
    }

    #[test]
    fn extension_fields_serialize_and_preserve_vocabulary() {
        let mut manifest = minimal_manifest();
        manifest.world = Some(WorldProvenance {
            world_content_sha256: Some("deadbeef".into()),
            bake_content_sha256: None,
            probe_batch_content_sha256: Some("feedface".into()),
        });
        manifest.stems.push(StemRecord {
            kind: StemKind::PathingOnSum,
            content_hash_sha256: Some("a".into()),
        });
        manifest.stems.push(StemRecord {
            kind: StemKind::PathingOffSum,
            content_hash_sha256: Some("b".into()),
        });
        manifest
            .source_calibrations
            .push(canonical_85db_calibration());
        manifest.pathing_toggle = Some(PathingToggleSummary {
            on_sum_hash_sha256: Some("a".into()),
            off_sum_hash_sha256: Some("b".into()),
            level_difference_db: Some(3.2),
            spectral_difference_l1: Some(0.01),
            differs: Some(true),
        });
        manifest.metrics.push(MetricRecord {
            label: "channel".into(),
            kind: "channel".into(),
            payload_json: r#"{"all_finite":true}"#.into(),
        });
        manifest.provenance = RunProvenance {
            engine_commit: Some("f00dface".into()),
            platform: Some("darwin".into()),
            cpu_class: Some("arm64".into()),
            hrtf_identity: Some("steam-audio-default".into()),
            fixture_content_sha256: Some("abc123".into()),
            bake_duration_s: Some(1.25),
            render_duration_s: Some(0.75),
            simulation_cadence: SimulationCadence {
                simulation_stride_frames: Some(1),
                snapshot_age_frames: Some(0),
                status: CadenceStatus::Fresh,
            },
            callback_timing: CallbackTiming {
                status: CallbackStatus::Met,
                deadline_fault_count: 0,
                max_callback_overrun_s: None,
            },
            limiter_events: vec![LimiterEvent {
                label: "output_ceiling".into(),
                gain_reduction_db: Some(-0.5),
                sample_frame: Some(12_345),
            }],
            degradation_events: vec![],
        };

        let json = manifest.to_json();
        // Original vocabulary is still present.
        assert!(json.contains(r#""fixture_id":"s3-corner""#));
        assert!(json.contains(r#""upstream_commit":"0da1825""#));
        // New vocabulary is present and deterministic.
        assert!(json.contains(r#""world_content_sha256":"deadbeef""#));
        assert!(json.contains(r#""probe_batch_content_sha256":"feedface""#));
        assert!(json.contains(r#""kind":"pathing_on_sum""#));
        assert!(json.contains(r#""level_difference_db":3.2"#));
        assert!(json.contains(r#""differs":true"#));
        assert!(json.contains(r#""payload":{"all_finite":true}}"#));
        // The complete one source-drive gain chain is recorded (ADR 0002).
        assert!(json.contains(r#""scene":{"reference_spl_db":120"#));
        assert!(json.contains(r#""reference_pcm_rms_dbfs":-24"#));
        assert!(json.contains(r#""reference_distance_m":1}"#));
        assert!(json.contains(r#""asset_analysis":{"program_rms_dbfs":-20"#));
        assert!(json.contains(r#""true_peak_dbtp":"#));
        assert!(json.contains(&format!(
            r#""measurement_method_id":"{ASSET_ANALYSIS_METHOD_ID}""#
        )));
        assert!(json.contains(
            r#""sample_rate_hz":48000,"channels":1,"channel_layout":"mono","frame_count":48000"#
        ));
        assert!(json.contains(r#""reference_level":{"mode":"SplAtOneMeter","value_db":85}"#));
        // Canonical ADR 0002 example: an 85 dB SPL source with -20 dBFS program
        // RMS derives a -59 dBFS target and a -39 dB drive.
        assert!(json.contains(r#""drive":{"target_source_rms_dbfs":-59"#));
        assert!(json.contains(r#""drive_gain_db":-39"#));
        // Generator normalization is recorded separately (here: not applicable).
        assert!(json.contains(r#""generator_normalization":null"#));
        // Monitor/output transfer is explicit and distinct from the drive.
        assert!(json.contains(r#""monitor":{"status":"not_applicable"}"#));
        // §ν provenance: engine build, host, durations, and runtime health.
        assert!(json.contains(r#""engine_commit":"f00dface""#));
        assert!(json.contains(r#""platform":"darwin""#));
        assert!(json.contains(r#""cpu_class":"arm64""#));
        assert!(json.contains(r#""hrtf_identity":"steam-audio-default""#));
        assert!(json.contains(r#""fixture_content_sha256":"abc123""#));
        assert!(json.contains(r#""bake_duration_s":1.25"#));
        assert!(json.contains(r#""render_duration_s":0.75"#));
        assert!(json.contains(r#""simulation_stride_frames":1"#));
        assert!(json.contains(r#""snapshot_age_frames":0"#));
        assert!(json.contains(r#""status":"fresh""#));
        assert!(json.contains(r#""deadline_fault_count":0"#));
        assert!(json.contains(r#""label":"output_ceiling""#));
        assert!(json.contains(r#""gain_reduction_db":-0.5"#));
        assert!(json.contains(r#""sample_frame":12345"#));
        assert!(json.contains(r#""degradation_events":[]"#));
        // Non-claims survive alongside the extension.
        assert!(json.contains(r#""non_claims":["S3 has not run"]"#));
        assert_eq!(json, manifest.to_json());
    }

    #[test]
    fn canonical_85db_source_derives_minus_59_target_and_minus_39_drive() {
        // ADR 0002's worked example: 85 dB SPL + -20 dBFS RMS asset -> -59 dBFS
        // target source RMS, -39 dB drive gain. The record is derived through
        // the single SceneCalibration::derive_source_drive chain.
        let record = canonical_85db_calibration();
        assert_eq!(record.drive.target_source_rms_dbfs, -59.0);
        assert_eq!(record.drive.drive_gain_db, -39.0);
        assert!(record.drive.expected_true_peak_dbtp.is_finite());
        assert!(record.drive.linear_gain > 0.0);
        assert_eq!(record.reference_level.mode, "SplAtOneMeter");
        assert_eq!(record.reference_level.value_db, 85.0);
        // Generator normalization is absent for a measured (non-generated) asset.
        assert_eq!(record.generator_normalization, None);
        // Monitor transfer is explicitly not applicable for an offline capture.
        assert_eq!(record.monitor, MonitorGainRecord::NotApplicable);
    }

    #[test]
    fn source_calibration_records_generator_normalization_and_monitor_applied() {
        // A generated asset records its generator normalization separately from
        // the source drive, and an applied monitor gain is recorded distinctly.
        let generated = sine(
            WavSpec {
                sample_rate_hz: 48_000,
                channels: 1,
            },
            1_000.0,
            48_000,
            -20.0,
        )
        .unwrap();
        let analyzed = generated.analyze().unwrap();
        let normalization = generated.normalization;
        let record = SourceCalibrationRecord::derive(
            "approach-source",
            SceneCalibration::default(),
            ReferenceLevel::SplAtOneMeter { db_spl: 85.0 },
            &analyzed,
            Some(normalization),
            MonitorGainRecord::Applied {
                monitor_gain_db: 0.0,
            },
        )
        .unwrap();

        assert_eq!(record.drive.target_source_rms_dbfs, -59.0);
        assert_eq!(record.drive.drive_gain_db, -39.0);
        // Generator normalization is recorded as the evidence-local fact, not
        // conflated with the physical source drive.
        let norm = record.generator_normalization.unwrap();
        assert!((norm.raw_rms_dbfs - (-3.0103)).abs() < 1e-3);
        assert!((norm.target_rms_dbfs - (-20.0)).abs() < 1e-3);
        // The two gains are visibly different numbers.
        assert_ne!(norm.normalization_gain_db, record.drive.drive_gain_db);
        assert_eq!(
            record.monitor,
            MonitorGainRecord::Applied {
                monitor_gain_db: 0.0,
            }
        );

        let json = format!("[{}]", calibrations_json(std::slice::from_ref(&record)));
        assert!(json.contains(r#""generator_normalization":{"raw_rms_dbfs":"#));
        assert!(json.contains(r#""target_rms_dbfs":-20"#));
        assert!(json.contains(r#""normalization_gain_db":"#));
        assert!(json.contains(r#""monitor":{"status":"applied","monitor_gain_db":0}"#));
    }

    #[test]
    fn source_calibration_creative_db_has_no_predicted_spl_in_record() {
        // CreativeDb sets drive gain = C and target = program_rms + C. It is
        // relative PCM gain only; the record carries it without an SPL claim.
        let analyzed = analyzed_minus_20_dbfs();
        let record = SourceCalibrationRecord::derive(
            "creative-source",
            SceneCalibration::default(),
            ReferenceLevel::CreativeDb { db: 6.0 },
            &analyzed,
            None,
            MonitorGainRecord::NotApplicable,
        )
        .unwrap();
        assert_eq!(record.reference_level.mode, "CreativeDb");
        assert_eq!(record.reference_level.value_db, 6.0);
        assert_eq!(record.drive.target_source_rms_dbfs, -14.0);
        assert_eq!(record.drive.drive_gain_db, 6.0);
    }

    #[test]
    fn derive_rejects_normalization_that_disagrees_with_analyzed_pcm() {
        let generated = sine(
            WavSpec {
                sample_rate_hz: 48_000,
                channels: 1,
            },
            1_000.0,
            48_000,
            -20.0,
        )
        .unwrap();
        let analyzed = generated.analyze().unwrap();
        let mut inconsistent = generated.normalization;
        inconsistent.target_rms_dbfs = -18.0;

        assert_eq!(
            SourceCalibrationRecord::derive(
                "bad-source",
                SceneCalibration::default(),
                ReferenceLevel::SplAtOneMeter { db_spl: 85.0 },
                &analyzed,
                Some(inconsistent),
                MonitorGainRecord::NotApplicable,
            )
            .unwrap_err(),
            SourceCalibrationError::InconsistentGeneratorNormalization
        );
    }

    #[test]
    fn completed_manifest_recomputes_and_rejects_inconsistent_drive() {
        // Private fields prevent public callers from creating this state. The
        // crate-internal mutation exercises the validation boundary directly.
        let mut record = canonical_85db_calibration();
        record.drive.drive_gain_db = 0.0;
        let mut manifest = minimal_manifest();
        manifest.state = RunState::Completed;
        manifest.source_calibrations.push(record);

        assert_eq!(
            manifest.validate(),
            Err(ManifestValidationError {
                source_index: 0,
                error: SourceCalibrationError::InconsistentDerivedDrive,
            })
        );
    }

    #[test]
    fn provenance_defaults_to_offline_not_applicable() {
        // A planned/offline manifest honestly reports that no streaming cadence
        // or realtime callback budget was exercised.
        let json = minimal_manifest().to_json();
        assert!(json.contains(r#""simulation_cadence":{"simulation_stride_frames":null,"snapshot_age_frames":null,"status":"not_applicable"}"#));
        assert!(json.contains(r#""callback_timing":{"status":"not_applicable","deadline_fault_count":0,"max_callback_overrun_s":null}"#));
        assert!(json.contains(r#""limiter_events":[]"#));
        assert!(json.contains(r#""degradation_events":[]"#));
        assert!(json.contains(r#""engine_commit":null"#));
    }

    #[test]
    fn degradation_events_serialize_in_order() {
        let mut manifest = minimal_manifest();
        manifest
            .provenance
            .degradation_events
            .push(DegradationEvent {
                label: "source_drop".into(),
                detail: "active sources exceeded the budget".into(),
                sample_frame: None,
            });
        let json = manifest.to_json();
        assert!(json.contains(r#""label":"source_drop""#));
        assert!(json.contains(r#""detail":"active sources exceeded the budget""#));
        assert!(json.contains(r#""sample_frame":null"#));
        assert_eq!(json, manifest.to_json());
    }
}
