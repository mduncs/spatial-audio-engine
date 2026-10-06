use std::path::Path;

use fightbox_api::atmosphere::{AtmosphereObservation, FALLBACK_ATMOSPHERE_OBSERVATION};
use fightbox_api::{Directivity, EnuVector3, ExtentDescriptor, ExtentError, ReferenceLevel};
use fightbox_steam_audio::{
    AcousticMaterial, BakedProbeBatch, DEFAULT_OCCLUSION_SAMPLE_COUNT,
    DEFAULT_OCCLUSION_SOURCE_RADIUS_METERS, DirectOcclusionMode,
    MAX_EXTENT_OCCLUSION_RADIUS_METERS, MIN_EXTENT_OCCLUSION_RADIUS_METERS,
    PROBE_BATCH_METADATA_SCHEMA, ProbeBatchMetadata, ReflectionEffectConfig, S3SimulationConfig,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SceneMesh,
};
use fightbox_world::LoadedPackage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
pub struct Fixture {
    #[serde(default)]
    pub fixture_id: Option<String>,
    #[serde(default)]
    pub audition: Option<FixtureAudition>,
    #[serde(default)]
    pub air: Option<FixtureAir>,
    pub sources: Vec<FixtureSource>,
    #[serde(default)]
    pub cues: Vec<FixtureCue>,
    /// Optional display-only street centerlines in local ENU metres (east, north).
    #[serde(default)]
    pub street_lines_m: Vec<Vec<[f32; 2]>>,
    /// Optional display-only street names, parallel to `street_lines_m` ("" is unnamed).
    #[serde(default)]
    pub street_names: Vec<String>,
    /// Optional display-only OSM `highway` kinds, parallel to `street_lines_m`.
    #[serde(default)]
    pub street_kinds: Vec<String>,
    pub listener: FixtureListener,
    pub simulation: FixtureSimulation,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum FixtureAir {
    Preset(AirPreset),
    Observation(FixtureAirObservation),
}

impl FixtureAir {
    pub const fn observation(self) -> AtmosphereObservation {
        match self {
            Self::Preset(preset) => preset.observation(),
            Self::Observation(observation) => AtmosphereObservation {
                temperature_c: observation.temperature_c,
                relative_humidity_percent: observation.relative_humidity_percent,
                pressure_kpa: observation.pressure_kpa,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AirPreset {
    #[default]
    Temperate,
    ColdDry,
    HotHumid,
    WarmDry,
}

impl AirPreset {
    pub const ALL: [Self; 4] = [Self::Temperate, Self::ColdDry, Self::HotHumid, Self::WarmDry];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Temperate => "Temperate",
            Self::ColdDry => "Cold dry",
            Self::HotHumid => "Hot humid",
            Self::WarmDry => "Warm dry",
        }
    }

    pub const fn observation(self) -> AtmosphereObservation {
        let (temperature_c, relative_humidity_percent) = match self {
            Self::Temperate => return FALLBACK_ATMOSPHERE_OBSERVATION,
            Self::ColdDry => (-5.0, 20.0),
            Self::HotHumid => (35.0, 80.0),
            Self::WarmDry => (30.0, 15.0),
        };
        AtmosphereObservation {
            temperature_c,
            relative_humidity_percent,
            pressure_kpa: FALLBACK_ATMOSPHERE_OBSERVATION.pressure_kpa,
        }
    }

    pub fn three_band_air_pressure_exponents_per_m(self) -> [f32; 3] {
        fightbox_runtime::FrozenAtmosphere::freeze(Some(self.observation()))
            .three_band_air_pressure_exponents_per_m()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FixtureAirObservation {
    pub temperature_c: f32,
    pub relative_humidity_percent: f32,
    #[serde(default = "default_air_pressure_kpa")]
    pub pressure_kpa: f32,
}

fn default_air_pressure_kpa() -> f32 {
    FALLBACK_ATMOSPHERE_OBSERVATION.pressure_kpa
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureCue {
    pub at_s: Option<f64>,
    pub when_listener_enters: Option<FixtureCueZone>,
    pub play: Option<String>,
    pub stop: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureCueZone {
    pub center_m: [f64; 2],
    pub radius_m: f64,
}

impl FixtureCue {
    pub fn source_id(&self) -> &str {
        self.play.as_deref().or(self.stop.as_deref()).unwrap_or("")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct FixtureSource {
    pub id: String,
    #[serde(default)]
    pub asset_id: String,
    #[serde(default)]
    pub live_input: Option<FixtureLiveInput>,
    /// Personal file path, resolved relative to the fixture or left absolute.
    #[serde(default)]
    pub program_file: Option<String>,
    #[serde(default)]
    pub audition_label: Option<String>,
    #[serde(default)]
    pub macro_range_m: Option<u32>,
    pub reference_level: FixtureReferenceLevel,
    /// Audition trim after the calibrated source drive.
    #[serde(default)]
    pub monitor_offset_db: f32,
    #[serde(default = "default_enabled")]
    pub default_enabled: bool,
    #[serde(default)]
    pub impulsive: bool,
    #[serde(default)]
    pub playback_start_offset_s: f64,
    /// When true, disabling the source rewinds its program so the next enable
    /// starts at frame zero. Existing fixtures default to free-running loops.
    #[serde(default)]
    pub restart_on_enable: bool,
    #[serde(default)]
    pub directivity: FixtureDirectivity,
    /// Optional horizontal emitter heading in ENU; moving trajectories replace
    /// this with their tangent after the first motion sample.
    #[serde(default = "default_forward_enu")]
    pub forward_enu: [f64; 3],
    #[serde(default, deserialize_with = "deserialize_extent")]
    pub extent: ExtentDescriptor,
    pub position_m: Option<[f64; 3]>,
    pub trajectory: Option<Trajectory>,
    /// Supersonic flight that ends at this source's static position. Each
    /// declaration adds one crack companion slot after the ordinary sources.
    #[serde(default)]
    pub ballistic: Option<FixtureBallistic>,
    /// A looping muzzle recording with a load-detected round clock. One
    /// companion slot renders all of its bullet shock waves.
    #[serde(default)]
    pub gunfire: Option<FixtureGunfire>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureGunfire {
    pub aim_point_m: [f64; 3],
    pub muzzle_velocity_mps: f64,
    /// Finite straight supersonic flight; no crack past its last tangent.
    pub supersonic_distance_m: f64,
    /// Deterministic horizontal spread at the aim point, in metres.
    pub dispersion_m: f64,
    /// Signed lateral aim-plane offsets, one per load-detected round. The
    /// schedule repeats with the muzzle loop; optional dispersion is added.
    #[serde(default)]
    pub round_aim_offsets_m: Option<Vec<f64>>,
    /// Send the same calibrated N-wave to the existing early Steam street
    /// response. Enabled by default; false is an explicit dry comparison.
    #[serde(default = "default_enabled")]
    pub street_response: bool,
    pub crack_peak_db_at_30_m: f64,
    #[serde(default)]
    pub n_wave_ms_at_30_m: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub notes: Option<String>,
}

impl FixtureGunfire {
    fn validate(&self, source: &FixtureSource) -> Result<(), String> {
        let bad = source.position_m.is_none() || source.trajectory.is_some()
            || source.ballistic.is_some() || source.live_input.is_some()
            || source.program_file.is_some() || !source.restart_on_enable
            || source.default_enabled || source.playback_start_offset_s != 0.0;
        if bad {
            return Err(format!("source {} gunfire needs a static, default-off, restart_on_enable muzzle asset at offset zero", source.id));
        }
        let muzzle = source.position_m.unwrap();
        let distance = (0..3).map(|i| (self.aim_point_m[i] - muzzle[i]).powi(2)).sum::<f64>().sqrt();
        if self.aim_point_m.iter().any(|x| !x.is_finite() || !(*x as f32).is_finite())
            || !distance.is_finite() || distance <= 0.0
            || !self.muzzle_velocity_mps.is_finite() || self.muzzle_velocity_mps <= 0.0
            || !self.supersonic_distance_m.is_finite() || self.supersonic_distance_m <= 0.0
            || self.supersonic_distance_m > 10_000.0
            || !self.dispersion_m.is_finite() || !(0.0..=5.0).contains(&self.dispersion_m)
            || self.round_aim_offsets_m.as_ref().is_some_and(|offsets|
                offsets.is_empty() || offsets.len() > 1024
                    || offsets.iter().any(|x| !x.is_finite() || x.abs() > 100.0))
            || !self.crack_peak_db_at_30_m.is_finite() || !(0.0..=200.0).contains(&self.crack_peak_db_at_30_m)
            || self.n_wave_ms_at_30_m.is_some_and(|x| !x.is_finite() || x <= 0.0 || x > 100.0)
        {
            return Err(format!("source {} gunfire requires finite aim, positive velocity/flight, spread in 0..=5 m and a valid crack anchor", source.id));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureLiveInput {
    pub device: String,
    #[serde(default)]
    pub channels: FixtureLiveInputChannels,
    /// Optional song the speaker plays until an app is chosen; relative to the fixture.
    #[serde(default)]
    pub song: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixtureLiveInputChannels {
    #[default]
    Mono,
    Stereo,
}

impl FixtureLiveInputChannels {
    #[must_use]
    pub const fn plane_count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }
}

/// Straight piecewise-Mach flight ending at the owning source's position.
///
/// `muzzle_position_m` is the start of the modelled flight, which may be a
/// terminal leg rather than a gun; no muzzle blast is rendered from it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureBallistic {
    pub muzzle_position_m: [f64; 3],
    pub mach_segments: Vec<FixtureMachSegment>,
    /// N-wave duration at a 30 m perpendicular miss. Defaults to the signed
    /// small-arms audition anchor.
    #[serde(default)]
    pub n_wave_ms_at_30_m: Option<f64>,
    /// Received crack peak level at a 30 m perpendicular miss.
    pub crack_peak_db_at_30_m: f64,
    /// Provenance for readers (for example, provisional physics values).
    #[serde(default)]
    #[allow(dead_code)]
    pub notes: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureMachSegment {
    pub length_m: f64,
    pub mach: f64,
}

/// Declared segment lengths must reach the impact within this distance.
const BALLISTIC_LENGTH_TOLERANCE_M: f64 = 0.1;

impl FixtureBallistic {
    fn validate(&self, source: &FixtureSource) -> Result<(), String> {
        let id = &source.id;
        let Some(impact) = source.position_m else {
            return Err(format!(
                "source {id} ballistic flight requires a static position_m impact"
            ));
        };
        if source.trajectory.is_some() {
            return Err(format!(
                "source {id} ballistic flight cannot end at a moving source"
            ));
        }
        if self
            .muzzle_position_m
            .iter()
            .any(|component| !component.is_finite() || !(*component as f32).is_finite())
        {
            return Err(format!(
                "source {id} ballistic muzzle_position_m must be finite"
            ));
        }
        if self.mach_segments.is_empty()
            || self.mach_segments.iter().any(|segment| {
                !segment.length_m.is_finite()
                    || segment.length_m <= 0.0
                    || !segment.mach.is_finite()
                    || segment.mach <= 0.0
            })
        {
            return Err(format!(
                "source {id} ballistic mach_segments need positive finite lengths and Mach"
            ));
        }
        let declared_m: f64 = self
            .mach_segments
            .iter()
            .map(|segment| segment.length_m)
            .sum();
        let straight_m = (0..3)
            .map(|axis| (impact[axis] - self.muzzle_position_m[axis]).powi(2))
            .sum::<f64>()
            .sqrt();
        if (declared_m - straight_m).abs() > BALLISTIC_LENGTH_TOLERANCE_M {
            return Err(format!(
                "source {id} ballistic mach_segments total {declared_m:.3} m but the flight to position_m is {straight_m:.3} m"
            ));
        }
        if self
            .n_wave_ms_at_30_m
            .is_some_and(|duration_ms| !duration_ms.is_finite() || duration_ms <= 0.0)
        {
            return Err(format!(
                "source {id} ballistic n_wave_ms_at_30_m must be positive"
            ));
        }
        if !self.crack_peak_db_at_30_m.is_finite()
            || !(0.0..=200.0).contains(&self.crack_peak_db_at_30_m)
        {
            return Err(format!(
                "source {id} ballistic crack_peak_db_at_30_m must lie in 0..=200 dB"
            ));
        }
        Ok(())
    }
}

fn default_enabled() -> bool {
    true
}

fn default_forward_enu() -> [f64; 3] {
    [0.0, 1.0, 0.0]
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureAudition {
    pub mode: String,
    pub title: String,
    pub place_label: String,
    pub place_nonclaim: String,
    pub macro_ranges_m: [u32; 3],
}

/// Strict JSON shape for a source-local Steam Audio dipole model.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FixtureDirectivity {
    pub dipole_weight: f64,
    pub dipole_power: f64,
}

impl FixtureDirectivity {
    fn validate(self, source_id: &str) -> Result<(), String> {
        if !self.dipole_weight.is_finite() {
            return Err(format!(
                "source {source_id} directivity.dipole_weight must be finite"
            ));
        }
        if !(f64::from(Directivity::MIN_DIPOLE_WEIGHT)..=f64::from(Directivity::MAX_DIPOLE_WEIGHT))
            .contains(&self.dipole_weight)
        {
            return Err(format!(
                "source {source_id} directivity.dipole_weight must be in [0,1]"
            ));
        }
        if !self.dipole_power.is_finite() {
            return Err(format!(
                "source {source_id} directivity.dipole_power must be finite"
            ));
        }
        if !(f64::from(Directivity::MIN_DIPOLE_POWER)..=f64::from(Directivity::MAX_DIPOLE_POWER))
            .contains(&self.dipole_power)
        {
            return Err(format!(
                "source {source_id} directivity.dipole_power must be in [0.25,16]"
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn to_api(self) -> Directivity {
        Directivity {
            dipole_weight: self.dipole_weight as f32,
            dipole_power: self.dipole_power as f32,
        }
    }
}

impl Default for FixtureDirectivity {
    fn default() -> Self {
        Self {
            dipole_weight: 0.0,
            dipole_power: 1.0,
        }
    }
}

/// Closed fixture JSON representation of [`ExtentDescriptor`].
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureExtentWire {
    Point {},
    MultiPoint { count: u8 },
    LineSegment { length_m: f64 },
    StereoImage { width_m: f64 },
}

fn deserialize_extent<'de, D>(deserializer: D) -> Result<ExtentDescriptor, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let extent = match FixtureExtentWire::deserialize(deserializer)? {
        FixtureExtentWire::Point {} => ExtentDescriptor::Point,
        FixtureExtentWire::MultiPoint { count } => ExtentDescriptor::MultiPoint { count },
        FixtureExtentWire::LineSegment { length_m } => ExtentDescriptor::LineSegment {
            length_m: length_m as f32,
        },
        FixtureExtentWire::StereoImage { width_m } => ExtentDescriptor::StereoImage {
            width_m: width_m as f32,
        },
    };
    Ok(extent)
}

fn validate_extent(extent: ExtentDescriptor, source_id: &str) -> Result<(), String> {
    extent.validate().map_err(|error| match error {
        ExtentError::EmptyMultiPoint => {
            format!("source {source_id} extent.count must be >= 1")
        }
        ExtentError::NonFiniteLineLength => {
            format!("source {source_id} extent.length_m must be finite")
        }
        ExtentError::NonPositiveLineLength => {
            format!("source {source_id} extent.length_m must be > 0")
        }
        ExtentError::NonFiniteStereoWidth => {
            format!("source {source_id} extent.width_m must be finite")
        }
        ExtentError::NonPositiveStereoWidth => {
            format!("source {source_id} extent.width_m must be > 0")
        }
    })
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum FixtureReferenceLevelMode {
    SplAtOneMeter,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureReferenceLevel {
    mode: FixtureReferenceLevelMode,
    pub db_spl: f64,
}

impl FixtureReferenceLevel {
    pub fn to_api(self) -> ReferenceLevel {
        debug_assert_eq!(self.mode, FixtureReferenceLevelMode::SplAtOneMeter);
        ReferenceLevel::SplAtOneMeter {
            db_spl: self.db_spl as f32,
        }
    }

    fn validate(self, source_id: &str) -> Result<(), String> {
        if !self.db_spl.is_finite() || !(self.db_spl as f32).is_finite() {
            return Err(format!(
                "source {source_id} reference_level.db_spl must be finite and representable as f32"
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Trajectory {
    pub waypoints_m: Vec<[f64; 3]>,
    pub speed_mps: f64,
    pub max_speed_mps: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FixtureListener {
    pub position_m: Option<[f64; 3]>,
    pub trajectory: Option<Trajectory>,
    pub forward_enu: [f64; 3],
}

#[derive(Clone, Debug, Deserialize)]
pub struct FixtureSimulation {
    pub direct: FixtureDirect,
    pub reflections: FixtureReflections,
    pub pathing: FixturePathing,
    pub probe_volume: FixtureProbeVolume,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixtureDirect {
    /// The current Workbench backend always applies this treatment. Parse the
    /// authored flag so unsupported bypass requests cannot silently do nothing.
    #[serde(default = "default_enabled")]
    pub distance_attenuation: bool,
    #[serde(default = "default_enabled")]
    pub occlusion: bool,
    pub occlusion_samples: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixtureReflections {
    /// Initial audible stage state. Simulation remains warm for live re-enabling.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub rays: Option<u32>,
    pub bounces: Option<u32>,
    pub duration_s: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixturePathing {
    pub order: Option<u32>,
    pub validation: Option<bool>,
    pub alternate_paths: Option<bool>,
    pub visibility_range_m: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixtureProbeVolume {
    pub spacing_m: f64,
}

/// Workbench path-range evidence carried into status output and captures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisibilityRangeAdoption {
    pub configured_m: f32,
    pub probe_spacing_m: f32,
    pub minimum_for_spacing_m: f32,
    pub effective_m: f32,
    pub rebaselined: bool,
}

impl Fixture {
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read fixture {}: {error}", path.display()))?;
        Self::parse(&bytes, &path.display().to_string())
    }

    pub(crate) fn parse(bytes: &[u8], source: &str) -> Result<Self, String> {
        let fixture: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid fixture {source}: {error}"))?;
        if let Some(air) = fixture.air {
            air.observation()
                .validate()
                .map_err(|error| format!("invalid fixture air: {error:?}"))?;
            if fixture.air_exponents().iter().any(|value| !value.is_finite() || *value < 0.0) {
                return Err("invalid fixture air: unrepresentable absorption coefficients".into());
            }
        }
        if fixture.sources.is_empty()
            || fixture.sources.len() > fightbox_runtime::MAX_ACTIVE_SOURCES
        {
            return Err("fixture must contain 1..=MAX_ACTIVE_SOURCES ordinary sources".into());
        }
        if fixture
            .street_lines_m
            .iter()
            .flatten()
            .flatten()
            .any(|coordinate| !coordinate.is_finite())
        {
            return Err("fixture street_lines_m coordinates must be finite".into());
        }
        for (field, len) in [
            ("street_names", fixture.street_names.len()),
            ("street_kinds", fixture.street_kinds.len()),
        ] {
            if len != 0 && len != fixture.street_lines_m.len() {
                return Err(format!("fixture {field} must be parallel to street_lines_m"));
            }
        }
        if fixture.runtime_source_count() > fightbox_runtime::MAX_ACTIVE_SOURCES {
            return Err(
                "fixture sources plus ballistic/gun crack slots exceed MAX_ACTIVE_SOURCES".into(),
            );
        }
        fixture.initial_listener_position()?;
        if fixture.cues.len() > 1024 {
            return Err("fixture cues must contain at most 1024 entries".into());
        }
        for (index, cue) in fixture.cues.iter().enumerate() {
            if cue.at_s.is_some() == cue.when_listener_enters.is_some()
                || cue.play.is_some() == cue.stop.is_some()
                || (cue.when_listener_enters.is_some() && cue.stop.is_some())
            {
                return Err(format!("cue {index} requires at_s with play/stop, or when_listener_enters with play"));
            }
            if cue.at_s.is_some_and(|time| {
                !time.is_finite() || time < 0.0 || time * 48_000.0 >= u64::MAX as f64
            }) {
                return Err(format!("cue {index} at_s must be finite, non-negative and fit the audio clock"));
            }
            if let Some(zone) = cue.when_listener_enters {
                if zone.center_m.iter().any(|value| !value.is_finite())
                    || !zone.radius_m.is_finite() || zone.radius_m <= 0.0
                {
                    return Err(format!("cue {index} zone requires a finite center_m and positive radius_m"));
                }
            }
            if !fixture.sources.iter().any(|source| source.id == cue.source_id()) {
                return Err(format!("cue {index} references unknown source {}", cue.source_id()));
            }
        }
        if !fixture.cues.is_empty() {
            let mut ids = std::collections::BTreeSet::new();
            if fixture.sources.iter().any(|source| !ids.insert(&source.id)) {
                return Err("cued scene source ids must be unique".into());
            }
        }
        if let Some(audition) = &fixture.audition {
            if !["gamma_audition", "squad_palette"].contains(&audition.mode.as_str())
                || audition.title.trim().is_empty()
                || audition.place_label.trim().is_empty()
                || audition.place_nonclaim.trim().is_empty()
                || audition.macro_ranges_m != [100, 1_000, 10_000]
                || fixture
                    .sources
                    .iter()
                    .any(|source| source.default_enabled || !source.restart_on_enable)
            {
                return Err(
                    "gamma audition metadata is incomplete or has invalid macro ranges".into(),
                );
            }
        }
        for source in &fixture.sources {
            if usize::from(!source.asset_id.trim().is_empty())
                + usize::from(source.live_input.is_some())
                + usize::from(source.program_file.is_some()) != 1
            {
                return Err(format!(
                    "source {} requires exactly one of asset_id, live_input or program_file",
                    source.id
                ));
            }
            if let Some(path) = &source.program_file {
                if path.trim().is_empty() || source.default_enabled || source.impulsive || source.ballistic.is_some() {
                    return Err(format!("source {} program_file must be non-empty, start off and use steady playback", source.id));
                }
            }
            if let Some(input) = &source.live_input {
                if input.device.trim().is_empty()
                    || input.song.as_ref().is_some_and(|song| song.trim().is_empty())
                {
                    return Err(format!(
                        "source {} live_input.device and song must be non-empty",
                        source.id
                    ));
                }
                match (input.channels, source.extent) {
                    (FixtureLiveInputChannels::Stereo, ExtentDescriptor::StereoImage { .. }) => {}
                    (FixtureLiveInputChannels::Stereo, _) => {
                        return Err(format!(
                            "source {} stereo live input requires StereoImage extent",
                            source.id
                        ));
                    }
                    (FixtureLiveInputChannels::Mono, _) => {}
                }
                if source.default_enabled
                    || source.impulsive
                    || source.ballistic.is_some()
                    || source.playback_start_offset_s != 0.0
                {
                    return Err(format!(
                        "source {} live input must start off and cannot use asset timing or ballistic playback",
                        source.id
                    ));
                }
            }
            source.initial_position()?;
            source.forward_enu_normalized()?;
            source.reference_level.validate(&source.id)?;
            if !source.monitor_offset_db.is_finite()
                || !(crate::mix_defaults::MIN_SOURCE_OFFSET_DB..=crate::mix_defaults::MAX_SOURCE_OFFSET_DB)
                    .contains(&source.monitor_offset_db)
            {
                return Err(format!("source {} monitor_offset_db is outside the supported trim range", source.id));
            }
            if source
                .audition_label
                .as_ref()
                .is_some_and(|label| label.trim().is_empty())
            {
                return Err(format!(
                    "source {} audition_label must be non-empty",
                    source.id
                ));
            }
            if source
                .macro_range_m
                .is_some_and(|range| ![100, 1_000, 10_000].contains(&range))
            {
                return Err(format!(
                    "source {} macro_range_m must be 100, 1000, or 10000",
                    source.id
                ));
            }
            if !source.playback_start_offset_s.is_finite() || source.playback_start_offset_s < 0.0 {
                return Err(format!(
                    "source {} playback_start_offset_s must be finite and non-negative",
                    source.id
                ));
            }
            source.directivity.validate(&source.id)?;
            validate_extent(source.extent, &source.id)?;
            if let Some(trajectory) = &source.trajectory {
                trajectory.validate(&format!("source {}", source.id))?;
            }
            if let Some(ballistic) = &source.ballistic {
                ballistic.validate(source)?;
            }
            if let Some(gunfire) = &source.gunfire {
                gunfire.validate(source)?;
            }
        }
        if let Some(trajectory) = &fixture.listener.trajectory {
            trajectory.validate("listener")?;
        }
        fixture.validate_simulation()?;
        Ok(fixture)
    }

    #[must_use]
    pub fn declared_source_count(&self) -> usize {
        self.sources.len().min(fightbox_runtime::MAX_ACTIVE_SOURCES)
    }

    /// Ordinary sources plus one crack companion slot per ballistic flight.
    #[must_use]
    pub fn runtime_source_count(&self) -> usize {
        self.declared_source_count()
            + self
                .sources
                .iter()
                .filter(|source| source.ballistic.is_some() || source.gunfire.is_some())
                .count()
    }

    pub fn initial_listener_position(&self) -> Result<EnuVector3, String> {
        initial_position(self.listener.position_m, self.listener.trajectory.as_ref())
            .ok_or_else(|| "listener requires a position or non-empty trajectory".into())
    }

    pub fn air_exponents(&self) -> [f32; 3] {
        fightbox_runtime::FrozenAtmosphere::freeze(self.air.map(FixtureAir::observation))
            .three_band_air_pressure_exponents_per_m()
    }

    pub fn simulation_config(&self) -> S3SimulationConfig {
        let visibility = self.visibility_range_adoption();
        let max_occlusion_samples = self.simulation.direct.occlusion_samples.unwrap_or(64) as i32;
        let occlusion_samples = self
            .simulation
            .direct
            .occlusion_samples
            .map_or(DEFAULT_OCCLUSION_SAMPLE_COUNT, |samples| samples as i32)
            .clamp(1, max_occlusion_samples);
        S3SimulationConfig {
            air_pressure_exponents_per_m: self.air_exponents(),
            max_occlusion_samples,
            direct_occlusion: DirectOcclusionMode::Volumetric {
                // Point sources retain the documented live-session footprint.
                // Non-point descriptors replace this radius inside the backend.
                radius_m: DEFAULT_OCCLUSION_SOURCE_RADIUS_METERS,
                sample_count: occlusion_samples,
            },
            reflection_rays: self.simulation.reflections.rays.unwrap_or(4_096) as i32,
            reflection_bounces: self.simulation.reflections.bounces.unwrap_or(2) as i32,
            reflection_duration_s: self.simulation.reflections.duration_s.unwrap_or(1.0) as f32,
            reflection_effect: ReflectionEffectConfig::CONVOLUTION,
            pathing_order: self.simulation.pathing.order.unwrap_or(2) as i32,
            pathing_visibility_range_m: visibility.effective_m,
            validate_paths: self.simulation.pathing.validation.unwrap_or(true),
            find_alternate_paths: self.simulation.pathing.alternate_paths.unwrap_or(true),
            trace_path_validation: false,
            ..S3SimulationConfig::default()
        }
    }

    /// Pairs runtime path visibility with the fixture's actual probe spacing.
    ///
    /// An absent configured range means the backend's existing 6 m default;
    /// the same 2.5x-spacing floor is then applied, so absence cannot silently
    /// recreate an under-ranged workbench session.
    pub fn visibility_range_adoption(&self) -> VisibilityRangeAdoption {
        let defaults = S3SimulationConfig::default();
        let configured_m =
            self.simulation
                .pathing
                .visibility_range_m
                .unwrap_or(f64::from(defaults.pathing_visibility_range_m)) as f32;
        let probe_spacing_m = self.simulation.probe_volume.spacing_m as f32;
        let minimum_for_spacing_m = probe_spacing_m * 2.5;
        let effective_m = configured_m.max(minimum_for_spacing_m);
        VisibilityRangeAdoption {
            configured_m,
            probe_spacing_m,
            minimum_for_spacing_m,
            effective_m,
            rebaselined: effective_m > configured_m,
        }
    }

    fn validate_simulation(&self) -> Result<(), String> {
        for (name, enabled) in [
            (
                "distance_attenuation",
                self.simulation.direct.distance_attenuation,
            ),
            ("occlusion", self.simulation.direct.occlusion),
        ] {
            if !enabled {
                return Err(format!(
                    "simulation.direct.{name}=false is not supported by the live Workbench backend; omit the field or set true (this treatment cannot currently be bypassed)"
                ));
            }
        }
        fn fits_i32(value: u32) -> bool {
            value <= i32::MAX as u32
        }

        if self
            .simulation
            .direct
            .occlusion_samples
            .is_some_and(|samples| samples == 0 || !fits_i32(samples))
        {
            return Err("simulation.direct.occlusion_samples must be in 1..=2147483647".into());
        }
        if self
            .simulation
            .reflections
            .rays
            .is_some_and(|rays| rays == 0 || !fits_i32(rays))
        {
            return Err("simulation.reflections.rays must be in 1..=2147483647".into());
        }
        if self
            .simulation
            .reflections
            .bounces
            .is_some_and(|bounces| !fits_i32(bounces))
        {
            return Err("simulation.reflections.bounces must be <= 2147483647".into());
        }
        if self
            .simulation
            .reflections
            .duration_s
            .is_some_and(|duration| {
                !duration.is_finite() || duration <= 0.0 || !(duration as f32).is_finite()
            })
        {
            return Err(
                "simulation.reflections.duration_s must be finite, positive, and representable as f32"
                    .into(),
            );
        }
        if self
            .simulation
            .pathing
            .order
            .is_some_and(|order| !fits_i32(order))
        {
            return Err("simulation.pathing.order must be <= 2147483647".into());
        }
        if self
            .simulation
            .pathing
            .visibility_range_m
            .is_some_and(|range| !range.is_finite() || range <= 0.0 || !(range as f32).is_finite())
        {
            return Err(
                "simulation.pathing.visibility_range_m must be finite, positive, and representable as f32"
                    .into(),
            );
        }
        let spacing = self.simulation.probe_volume.spacing_m;
        if !spacing.is_finite()
            || spacing <= 0.0
            || !(spacing as f32).is_finite()
            || !((spacing as f32) * 2.5).is_finite()
        {
            return Err(
                "simulation.probe_volume.spacing_m must be finite, positive, and support the 2.5x visibility guard"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Resolves the per-source volumetric request the backend derives from the
/// descriptor passed by the workbench. Kept here for status/capture truth; the
/// backend remains authoritative and consumes the same extent independently.
pub fn occlusion_mode_for_extent(
    config: S3SimulationConfig,
    extent: ExtentDescriptor,
) -> DirectOcclusionMode {
    let radius_m = match extent {
        ExtentDescriptor::Point => return config.direct_occlusion,
        ExtentDescriptor::MultiPoint { count } if count > 0 => {
            DEFAULT_OCCLUSION_SOURCE_RADIUS_METERS
        }
        ExtentDescriptor::LineSegment { length_m }
        | ExtentDescriptor::StereoImage { width_m: length_m }
            if length_m.is_finite() && length_m > 0.0 =>
        {
            (length_m * 0.5).clamp(
                MIN_EXTENT_OCCLUSION_RADIUS_METERS,
                MAX_EXTENT_OCCLUSION_RADIUS_METERS,
            )
        }
        _ => return config.direct_occlusion,
    };
    let sample_count = match config.direct_occlusion {
        DirectOcclusionMode::Raycast => {
            DEFAULT_OCCLUSION_SAMPLE_COUNT.min(config.max_occlusion_samples.max(1))
        }
        DirectOcclusionMode::Volumetric { sample_count, .. } => sample_count,
    };
    DirectOcclusionMode::Volumetric {
        radius_m,
        sample_count,
    }
}

impl FixtureSource {
    /// The song decoded at startup: a program file, or a live speaker's default song.
    pub fn song_file(&self) -> Option<&str> {
        self.program_file
            .as_deref()
            .or_else(|| self.live_input.as_ref()?.song.as_deref())
    }

    pub fn initial_position(&self) -> Result<EnuVector3, String> {
        initial_position(self.position_m, self.trajectory.as_ref())
            .ok_or_else(|| format!("source {} requires a position or trajectory", self.id))
    }

    pub fn forward_enu_normalized(&self) -> Result<EnuVector3, String> {
        let [east, north, up] = self.forward_enu;
        if !east.is_finite() || !north.is_finite() || !up.is_finite() || (up.abs() > 1.0e-6) {
            return Err(format!(
                "source {} forward_enu must be finite and horizontal",
                self.id
            ));
        }
        let length = east.hypot(north);
        if !length.is_finite() || length <= f64::EPSILON {
            return Err(format!("source {} forward_enu must be non-zero", self.id));
        }
        Ok(EnuVector3::new(
            (east / length) as f32,
            (north / length) as f32,
            0.0,
        ))
    }
}

impl Trajectory {
    fn validate(&self, owner: &str) -> Result<(), String> {
        if self.waypoints_m.len() < 2 {
            return Err(format!(
                "{owner} trajectory requires at least two waypoints"
            ));
        }
        if !self.speed_mps.is_finite()
            || self.speed_mps <= 0.0
            || !(self.speed_mps as f32).is_finite()
        {
            return Err(format!("{owner} trajectory speed_mps must be positive"));
        }
        if let Some(max_speed_mps) = self.max_speed_mps
            && (!max_speed_mps.is_finite()
                || max_speed_mps <= 0.0
                || !(max_speed_mps as f32).is_finite()
                || self.speed_mps > max_speed_mps)
        {
            return Err(format!(
                "{owner} trajectory speed_mps must not exceed max_speed_mps"
            ));
        }
        if self
            .waypoints_m
            .iter()
            .flatten()
            .any(|component| !component.is_finite() || !(*component as f32).is_finite())
        {
            return Err(format!("{owner} trajectory waypoints must be finite"));
        }
        Ok(())
    }
}

fn initial_position(
    position: Option<[f64; 3]>,
    trajectory: Option<&Trajectory>,
) -> Option<EnuVector3> {
    position
        .or_else(|| trajectory?.waypoints_m.first().copied())
        .map(to_enu)
        .filter(|position| position.is_finite())
}

fn to_enu(value: [f64; 3]) -> EnuVector3 {
    EnuVector3::new(value[0] as f32, value[1] as f32, value[2] as f32)
}

pub fn scene_mesh(package: &LoadedPackage) -> Result<SceneMesh, String> {
    let triangles = package
        .mesh
        .triangles
        .iter()
        .map(|triangle| {
            Ok([
                i32::try_from(triangle[0]).map_err(|_| "mesh index exceeds i32")?,
                i32::try_from(triangle[1]).map_err(|_| "mesh index exceeds i32")?,
                i32::try_from(triangle[2]).map_err(|_| "mesh index exceeds i32")?,
            ])
        })
        .collect::<Result<Vec<_>, String>>()?;
    let material_indices = package
        .mesh
        .material_ids
        .iter()
        .map(|index| i32::try_from(*index).map_err(|_| "material index exceeds i32".into()))
        .collect::<Result<Vec<_>, String>>()?;
    let materials = package
        .materials
        .iter()
        .map(|(_, material)| AcousticMaterial {
            absorption: material.absorption,
            scattering: material.scattering,
            transmission: material.transmission,
        })
        .collect();
    Ok(SceneMesh {
        vertices_enu_m: package
            .mesh
            .vertices_enu_m
            .iter()
            .map(|vertex| {
                fightbox_steam_audio::EnuVector3::new(vertex.east_m, vertex.north_m, vertex.up_m)
            })
            .collect(),
        triangles,
        material_indices,
        materials,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeMetadataWire {
    schema_version: String,
    steam_audio_version: String,
    upstream_commit: String,
    probe_count: u32,
    path_data_size_bytes: u64,
    serialized_size_bytes: u64,
    content_sha256: String,
    bake_progress_callback_count: u32,
    final_bake_progress_millionths: u32,
}

pub fn load_baked(path: &Path, package: &LoadedPackage) -> Result<BakedProbeBatch, String> {
    let bytes = std::fs::read(path.join("probe-batch.bin"))
        .map_err(|error| format!("cannot read probe batch: {error}"))?;
    let metadata_text = std::fs::read_to_string(path.join("probe-batch-metadata.json"))
        .map_err(|error| format!("cannot read probe metadata: {error}"))?;
    let wire: ProbeMetadataWire = serde_json::from_str(&metadata_text)
        .map_err(|error| format!("invalid probe metadata: {error}"))?;
    if wire.schema_version != PROBE_BATCH_METADATA_SCHEMA
        || wire.steam_audio_version != STEAM_AUDIO_VERSION
        || wire.upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT
    {
        return Err("probe metadata does not match the Steam Audio backend".into());
    }
    let baked = BakedProbeBatch {
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: wire.probe_count,
            path_data_size_bytes: wire.path_data_size_bytes,
            serialized_size_bytes: wire.serialized_size_bytes,
            content_sha256: wire.content_sha256,
            bake_progress_callback_count: wire.bake_progress_callback_count,
            final_bake_progress_millionths: wire.final_bake_progress_millionths,
        },
        bytes,
    };
    baked
        .validate()
        .map_err(|error| format!("invalid baked probe batch: {error}"))?;
    verify_bake_identity(path, package, &baked)?;
    Ok(baked)
}

fn verify_bake_identity(
    path: &Path,
    package: &LoadedPackage,
    baked: &BakedProbeBatch,
) -> Result<(), String> {
    let manifest_path = path.join("city-bake-manifest.json");
    if !manifest_path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(&manifest_path)
        .map_err(|error| format!("cannot read city bake manifest: {error}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid city bake manifest: {error}"))?;
    for (field, expected) in [
        (
            "mesh_content_sha256",
            package.manifest.mesh_content_sha256.as_str(),
        ),
        (
            "materials_content_sha256",
            package.manifest.materials_content_sha256.as_str(),
        ),
        ("probe_batch_sha256", baked.metadata.content_sha256.as_str()),
    ] {
        if value.get(field).and_then(Value::as_str) != Some(expected) {
            return Err(format!("bake was produced from another package ({field})"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_air_defaults_to_bit_exact_fallback() {
        let fixture = Fixture::parse(include_bytes!(
            "../../../fixtures/city/astra-artillery/street-path-candidate.json"
        ), "default-air").unwrap();
        assert!(fixture.air.is_none());
        assert_eq!(
            fixture.air_exponents().map(f32::to_bits),
            fightbox_runtime::FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M.map(f32::to_bits)
        );
        assert_eq!(
            AirPreset::Temperate.three_band_air_pressure_exponents_per_m().map(f32::to_bits),
            fixture.air_exponents().map(f32::to_bits)
        );
    }

    #[test]
    fn fixture_air_presets_follow_iso_frequency_order() {
        for preset in AirPreset::ALL {
            assert_eq!(preset.observation().validate(), Ok(()));
            let exponents = preset.three_band_air_pressure_exponents_per_m();
            assert!(exponents.iter().all(|value| value.is_finite() && *value > 0.0));
            assert!(exponents[0] < exponents[1] && exponents[1] < exponents[2]);
            println!("{}: {exponents:?}", preset.label());
        }
        let upper = |preset: AirPreset| preset.three_band_air_pressure_exponents_per_m()[2];
        // ISO relaxation loss depends on humidity and frequency, not dryness alone.
        assert!(upper(AirPreset::ColdDry) < upper(AirPreset::HotHumid));
        assert!(upper(AirPreset::HotHumid) < upper(AirPreset::Temperate));
        assert!(upper(AirPreset::Temperate) < upper(AirPreset::WarmDry));
    }

    #[test]
    fn fixture_air_parse_validate_and_roundtrip() {
        let mut wire: Value = serde_json::from_slice(include_bytes!(
            "../../../fixtures/city/astra-artillery/street-path-candidate.json"
        )).unwrap();
        for air in AirPreset::ALL.map(FixtureAir::Preset).into_iter().chain([
            FixtureAir::Observation(FixtureAirObservation {
                temperature_c: 15.0,
                relative_humidity_percent: 65.0,
                pressure_kpa: 98.0,
            }),
        ]) {
            wire["air"] = serde_json::to_value(air).unwrap();
            let fixture = Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "air-roundtrip").unwrap();
            assert_eq!(fixture.air, Some(air));
            assert_eq!(fixture.simulation_config().air_pressure_exponents_per_m, fixture.air_exponents());
            assert_eq!(serde_json::to_value(fixture.air.unwrap()).unwrap(), wire["air"]);
        }
        wire["air"] = serde_json::json!({"temperature_c": 20, "relative_humidity_percent": 50});
        let fixture = Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "air-pressure-default").unwrap();
        assert_eq!(fixture.air.unwrap().observation(), FALLBACK_ATMOSPHERE_OBSERVATION);
        assert_eq!(fixture.air_exponents().map(f32::to_bits), AirPreset::Temperate.three_band_air_pressure_exponents_per_m().map(f32::to_bits));
        for air in [
            serde_json::json!("rainy"),
            serde_json::json!({"temperature_c": -21, "relative_humidity_percent": 50}),
            serde_json::json!({"temperature_c": 20, "relative_humidity_percent": 101}),
            serde_json::json!({"temperature_c": 20, "relative_humidity_percent": 50, "pressure_kpa": 0}),
            serde_json::json!({"temperature_c": 20, "relative_humidity_percent": 50, "pressure_kpa": 1e-40}),
            serde_json::json!({"temperature_c": 20, "relative_humidity_percent": 50, "wind_mps": 1}),
        ] {
            wire["air"] = air;
            assert!(Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "bad-air").is_err());
        }
    }

    #[test]
    fn scene_cues_parse_and_reject_invalid_targets_and_triggers() {
        let mut wire: Value = serde_json::from_str(include_str!(
            "../../../fixtures/city/astra-artillery/street-path-candidate.json"
        )).unwrap();
        wire["cues"] = serde_json::json!([
            {"at_s": 12.5, "play": "artillery-corner-shot"},
            {"at_s": 30, "stop": "artillery-corner-shot"},
            {"when_listener_enters": {"center_m": [4,5], "radius_m": 2}, "play": "artillery-corner-shot"}
        ]);
        let parsed = Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "cues").unwrap();
        assert_eq!(parsed.cues.len(), 3);
        assert_eq!(parsed.cues[0].at_s, Some(12.5));
        for (cue, message) in [
            (serde_json::json!({"at_s": -1, "play": "artillery-corner-shot"}), "non-negative"),
            (serde_json::json!({"at_s": 1, "play": "missing"}), "unknown source"),
            (serde_json::json!({"at_s": 1, "play": "artillery-corner-shot", "stop": "artillery-corner-shot"}), "requires"),
            (serde_json::json!({"when_listener_enters": {"center_m": [0,0], "radius_m": 0}, "play": "artillery-corner-shot"}), "positive radius"),
        ] {
            wire["cues"] = serde_json::json!([cue]);
            let error = Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "bad-cue").unwrap_err();
            assert!(error.contains(message), "{error}");
        }
    }

    #[test]
    fn scene_combat_reference_loads_within_voice_budget() {
        let fixture = Fixture::parse(include_bytes!(
            "../../../fixtures/city/combat-reference/fixture.json"
        ), "combat").unwrap();
        assert_eq!(fixture.sources.len(), 7);
        assert_eq!(fixture.runtime_source_count(), 8);
        assert!(!fixture.cues.is_empty());
        assert!(fixture.sources.iter().all(|source| !source.default_enabled));
    }

    #[test]
    fn live_music_fixture_accepts_live_binding_and_rejects_asset_overlap() {
        let bytes = include_bytes!("../../../fixtures/city/live-music/fixture.json");
        let fixture = Fixture::parse(bytes, "live-music").unwrap();
        assert_eq!(
            fixture.sources[0].live_input.as_ref().unwrap().device,
            "All system audio"
        );
        assert_eq!(
            fixture.sources[0].live_input.as_ref().unwrap().channels,
            FixtureLiveInputChannels::Stereo
        );
        assert_eq!(
            fixture.sources[0].extent,
            ExtentDescriptor::StereoImage { width_m: 4.0 }
        );
        assert!(fixture.sources[0].asset_id.is_empty());
        assert!(!fixture.sources[0].default_enabled);
        let mut wire: Value = serde_json::from_slice(bytes).unwrap();
        wire["sources"][0]["asset_id"] = "toms-diner".into();
        assert!(Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "overlap").is_err());
        wire["sources"][0]
            .as_object_mut()
            .unwrap()
            .remove("asset_id");
        wire["sources"][0]["default_enabled"] = true.into();
        assert!(Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "auto-play").is_err());

        let mut wire: Value = serde_json::from_slice(bytes).unwrap();
        wire["sources"][0]["live_input"]["channels"] = "mono".into();
        wire["sources"][0]["live_input"]
            .as_object_mut()
            .unwrap()
            .remove("channels");
        let mono = Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "default-mono").unwrap();
        assert_eq!(
            mono.sources[0].live_input.as_ref().unwrap().channels,
            FixtureLiveInputChannels::Mono
        );
        wire["sources"][0]["live_input"]["channels"] = "stereo".into();
        wire["sources"][0].as_object_mut().unwrap().remove("extent");
        assert!(
            Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "stereo-point-extent").is_err()
        );
    }

    #[test]
    fn reflection_output_enable_is_parsed_and_defaults_to_true() {
        let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../fixtures/city/chicago-walk/fixture.json"
        ))
        .unwrap();
        for enabled in [false, true] {
            value["simulation"]["reflections"]["enabled"] = enabled.into();
            let fixture =
                Fixture::parse(&serde_json::to_vec(&value).unwrap(), "reflection-enable").unwrap();
            assert_eq!(fixture.simulation.reflections.enabled, enabled);
        }
        value["simulation"]["reflections"]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
        let fixture =
            Fixture::parse(&serde_json::to_vec(&value).unwrap(), "reflection-default").unwrap();
        assert!(fixture.simulation.reflections.enabled);
    }

    #[test]
    fn chicago_fixture_starts_at_first_listener_waypoint() {
        let fixture = Fixture::read(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/city/chicago-walk/fixture.json"),
        )
        .unwrap();
        assert_eq!(
            fixture.initial_listener_position().unwrap(),
            EnuVector3::new(15.5, -55.0, 1.5)
        );
        assert_eq!(
            fixture.sources[0].initial_position().unwrap(),
            EnuVector3::new(12.5, -12.0, 1.5)
        );
        assert_eq!(
            fixture.sources[0].directivity,
            FixtureDirectivity::default()
        );
        assert_eq!(fixture.sources[0].extent, ExtentDescriptor::Point);
    }

    #[test]
    fn workbench_fixture_loader_accepts_sixteen_sources_and_rejects_seventeen() {
        let schema: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../../fixtures/workbench.schema.json"))
                .unwrap();
        assert_eq!(
            schema
                .pointer("/properties/sources/maxItems")
                .and_then(Value::as_u64),
            Some(16)
        );

        let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../fixtures/city/chicago-walk/fixture.json"
        ))
        .unwrap();
        let sources = value["sources"].as_array_mut().unwrap();
        let template = sources[0].clone();
        while sources.len() < fightbox_runtime::MAX_ACTIVE_SOURCES {
            let mut source = template.clone();
            source["id"] = format!("synthetic-source-{}", sources.len()).into();
            sources.push(source);
        }
        let sixteen = serde_json::to_vec(&value).unwrap();
        let fixture = Fixture::parse(&sixteen, "synthetic-sixteen-source-fixture").unwrap();
        assert_eq!(fixture.sources.len(), 16);
        assert_eq!(fixture.declared_source_count(), 16);

        let sources = value["sources"].as_array_mut().unwrap();
        let mut seventeenth = template;
        seventeenth["id"] = "synthetic-source-16".into();
        sources.push(seventeenth);
        let seventeen = serde_json::to_vec(&value).unwrap();
        assert!(Fixture::parse(&seventeen, "synthetic-seventeen-source-fixture").is_err());
    }

    #[test]
    fn fixture_source_restart_on_enable_is_explicit_and_defaults_false() {
        let original = include_str!("../../../fixtures/city/megablock/fixture.json");
        let text = original.replacen(
            r#""asset_id": "toms-diner","#,
            r#""asset_id": "toms-diner", "restart_on_enable": true,"#,
            1,
        );
        let fixture = Fixture::parse(text.as_bytes(), "restart-on-enable-test").unwrap();
        assert!(fixture.sources[0].restart_on_enable);
        assert!(!fixture.sources[1].restart_on_enable);
    }

    #[test]
    fn fixture_source_accepts_present_directivity() {
        let text = include_str!("../../../fixtures/city/chicago-walk/fixture.json").replace(
            r#""position_m": [12.5, -12.0, 1.5]"#,
            r#""directivity": {"dipole_weight": 0.75, "dipole_power": 2.0},
      "position_m": [12.5, -12.0, 1.5]"#,
        );
        let fixture = Fixture::parse(text.as_bytes(), "directivity-test").unwrap();
        assert_eq!(
            fixture.sources[0].directivity,
            FixtureDirectivity {
                dipole_weight: 0.75,
                dipole_power: 2.0,
            }
        );
        assert_eq!(
            fixture.sources[0].directivity.to_api(),
            Directivity {
                dipole_weight: 0.75,
                dipole_power: 2.0,
            }
        );
    }

    #[test]
    fn fixture_source_rejects_invalid_and_unknown_directivity_shapes() {
        for (directivity, expected) in [
            (
                r#"{"dipole_weight": -0.1, "dipole_power": 2.0}"#,
                "dipole_weight must be in [0,1]",
            ),
            (
                r#"{"dipole_weight": 0.75, "dipole_power": 16.1}"#,
                "dipole_power must be in [0.25,16]",
            ),
            (
                r#"{"dipole_weight": 0.75, "dipole_power": 2.0, "axis": "north"}"#,
                "unknown field `axis`",
            ),
            (r#"{"dipole_weight": 0.75}"#, "missing field `dipole_power`"),
        ] {
            let text = include_str!("../../../fixtures/city/chicago-walk/fixture.json").replace(
                r#""position_m": [12.5, -12.0, 1.5]"#,
                &format!(
                    r#""directivity": {directivity},
      "position_m": [12.5, -12.0, 1.5]"#
                ),
            );
            let error = Fixture::parse(text.as_bytes(), "directivity-test").unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?} in directivity error, got: {error}"
            );
        }
    }

    #[test]
    fn fixture_source_accepts_all_extent_kinds_and_defaults_absent_to_point() {
        let fixture = Fixture::parse(
            include_bytes!("../../../fixtures/city/chicago-walk/fixture.json"),
            "extent-default-test",
        )
        .unwrap();
        assert_eq!(fixture.sources[0].extent, ExtentDescriptor::Point);

        for (extent, expected) in [
            (r#"{"kind": "point"}"#, ExtentDescriptor::Point),
            (
                r#"{"kind": "multi_point", "count": 3}"#,
                ExtentDescriptor::MultiPoint { count: 3 },
            ),
            (
                r#"{"kind": "line_segment", "length_m": 6.0}"#,
                ExtentDescriptor::LineSegment { length_m: 6.0 },
            ),
            (
                r#"{"kind": "stereo_image", "width_m": 4.0}"#,
                ExtentDescriptor::StereoImage { width_m: 4.0 },
            ),
        ] {
            let text = include_str!("../../../fixtures/city/chicago-walk/fixture.json").replace(
                r#""position_m": [12.5, -12.0, 1.5]"#,
                &format!(
                    r#""extent": {extent},
      "position_m": [12.5, -12.0, 1.5]"#
                ),
            );
            let fixture = Fixture::parse(text.as_bytes(), "extent-test").unwrap();
            assert_eq!(fixture.sources[0].extent, expected);
        }
    }

    #[test]
    fn fixture_source_rejects_invalid_and_unknown_extent_shapes() {
        for (extent, expected) in [
            (
                r#"{"kind": "multi_point", "count": 0}"#,
                "extent.count must be >= 1",
            ),
            (
                r#"{"kind": "line_segment", "length_m": 0.0}"#,
                "extent.length_m must be > 0",
            ),
            (
                r#"{"kind": "stereo_image", "width_m": -1.0}"#,
                "extent.width_m must be > 0",
            ),
            (r#"{"kind": "line_segment"}"#, "missing field `length_m`"),
            (r#"{"kind": "point", "count": 1}"#, "unknown field `count`"),
        ] {
            let text = include_str!("../../../fixtures/city/chicago-walk/fixture.json").replace(
                r#""position_m": [12.5, -12.0, 1.5]"#,
                &format!(
                    r#""extent": {extent},
      "position_m": [12.5, -12.0, 1.5]"#
                ),
            );
            let error = Fixture::parse(text.as_bytes(), "extent-test").unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?} in extent error, got: {error}"
            );
        }
    }

    #[test]
    fn megablock_fixture_matches_the_synthesized_grid_frame() {
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/city/megablock/fixture.json");
        let fixture = Fixture::read(&fixture_path).unwrap();
        assert_eq!(
            fixture.initial_listener_position().unwrap(),
            EnuVector3::new(197.5, 292.5, 1.5)
        );
        assert_eq!(
            fixture.sources[0].initial_position().unwrap(),
            EnuVector3::new(292.5, 292.5, 1.5)
        );
        assert_eq!(fixture.sources.len(), 5);
        assert_eq!(fixture.declared_source_count(), 5);
        assert!(fixture.sources[0].default_enabled);
        assert_eq!(fixture.sources[0].reference_level.db_spl, 105.0);
        assert_eq!(
            fixture.sources[0].reference_level.to_api(),
            ReferenceLevel::SplAtOneMeter { db_spl: 105.0 }
        );
        assert_eq!(
            fixture.sources[0].directivity,
            FixtureDirectivity::default()
        );
        assert_eq!(fixture.sources[0].extent, ExtentDescriptor::Point);
        assert_eq!(fixture.sources[1].asset_id, "artillery-impact");
        assert_eq!(fixture.sources[1].reference_level.db_spl, 155.0);
        // Launch default (md, 2026-08-02): only Tom's Diner starts enabled;
        // everything else is opt-in via checkbox or a saved mix-defaults file.
        assert!(!fixture.sources[1].default_enabled);
        assert_eq!(
            fixture.sources[1].directivity,
            FixtureDirectivity::default()
        );
        assert_eq!(
            fixture.sources[1].extent,
            ExtentDescriptor::LineSegment { length_m: 6.0 }
        );
        assert_eq!(
            fixture.sources[1].initial_position().unwrap(),
            EnuVector3::new(102.5, 102.5, 1.5)
        );
        assert_eq!(fixture.sources[2].asset_id, "ff-siren");
        assert_eq!(fixture.sources[2].reference_level.db_spl, 118.0);
        assert!(!fixture.sources[2].default_enabled);
        assert_eq!(
            fixture.sources[2].directivity,
            FixtureDirectivity {
                dipole_weight: 0.5,
                dipole_power: 2.0,
            }
        );
        assert_eq!(fixture.sources[2].extent, ExtentDescriptor::Point);
        assert_eq!(
            fixture.sources[2].initial_position().unwrap(),
            EnuVector3::new(245.0, 245.0, 1.5)
        );
        assert_eq!(
            fixture.sources[2].trajectory.as_ref().unwrap().speed_mps,
            8.0
        );
        assert_eq!(fixture.sources[3].asset_id, "church-bells");
        assert_eq!(fixture.sources[3].reference_level.db_spl, 115.0);
        assert!(!fixture.sources[3].default_enabled);
        assert_eq!(
            fixture.sources[3].directivity,
            FixtureDirectivity::default()
        );
        assert_eq!(fixture.sources[3].extent, ExtentDescriptor::Point);
        assert_eq!(
            fixture.sources[3].initial_position().unwrap(),
            EnuVector3::new(482.5, 292.5, 60.0)
        );
        assert_eq!(fixture.sources[4].id, "dshk-street-gun");
        assert!(!fixture.sources[4].default_enabled);
        assert!(fixture.sources[4].impulsive);
        assert!(fixture.sources[..4].iter().all(|source| !source.impulsive));
        assert_eq!(fixture.sources[4].asset_id, "squad-dshk-burst-loop");
        assert_eq!(fixture.sources[4].reference_level.db_spl, 154.0);
        assert_eq!(
            fixture.sources[4].extent,
            ExtentDescriptor::LineSegment { length_m: 2.0 }
        );
        assert_eq!(
            fixture.sources[4].initial_position().unwrap(),
            EnuVector3::new(30.0, 288.0, 1.5)
        );
        let text = std::fs::read_to_string(fixture_path).unwrap();
        assert!(text.contains("4a614d600d4ef66a98923598a790e9b7054e4b8722af79f84fa82a0c6a0ee843"));
    }

    #[test]
    fn ballistic_flight_must_end_at_a_static_source_and_fit_the_slots() {
        let street =
            include_str!("../../../fixtures/city/astra-artillery/street-path-candidate.json");
        let fixture = Fixture::parse(street.as_bytes(), "street").unwrap();
        let ballistic = fixture.sources[0].ballistic.as_ref().unwrap();
        assert_eq!(ballistic.mach_segments.len(), 1);
        assert_eq!(ballistic.n_wave_ms_at_30_m, Some(2.8));
        assert_eq!(ballistic.crack_peak_db_at_30_m, 150.8);
        assert_eq!(fixture.runtime_source_count(), 2);

        let mut value: Value = serde_json::from_str(street).unwrap();
        let reject = |value: &Value, expected: &str| {
            let error = Fixture::parse(value.to_string().as_bytes(), "edited").unwrap_err();
            assert!(error.contains(expected), "{error}");
        };
        let flight = "/sources/0/ballistic";
        let mut short = value.clone();
        *short
            .pointer_mut(&format!("{flight}/mach_segments/0/length_m"))
            .unwrap() = 2_999.0.into();
        reject(&short, "mach_segments total");
        let mut typo = value.clone();
        typo.pointer_mut(flight)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("mach_segment".into(), Value::Null);
        reject(&typo, "unknown field");
        let mut moving = value.clone();
        moving.pointer_mut("/sources/0").unwrap().as_object_mut().unwrap().insert(
            "trajectory".into(),
            serde_json::json!({"waypoints_m": [[102.5, 102.5, 1.5], [110.0, 102.5, 1.5]], "speed_mps": 1.0}),
        );
        reject(&moving, "moving source");

        let source = value.pointer("/sources/0").unwrap().clone();
        let sources = value
            .pointer_mut("/sources")
            .unwrap()
            .as_array_mut()
            .unwrap();
        for index in 1..=8 {
            let mut copy = source.clone();
            copy["id"] = format!("shot-{index}").into();
            sources.push(copy);
        }
        reject(&value, "exceed MAX_ACTIVE_SOURCES");
    }

    #[test]
    fn fixtures_carrying_a_retired_events_block_still_load() {
        let original = include_str!("../../../fixtures/city/megablock/fixture.json");
        let event = r#""events": [{
    "id": "test-supersonic-shot",
    "kind": "ballistic_shot",
    "trigger_key": "space",
    "muzzle_m": [30.0, 288.0, 1.5]
  }],
  "#;
        let text = original.replace(r#""listener": {"#, &format!("{event}\"listener\": {{"));
        let fixture = Fixture::parse(text.as_bytes(), "test-retired-events-block").unwrap();

        assert_eq!(fixture.sources.len(), 5);
        assert_eq!(fixture.declared_source_count(), 5);
    }

    #[test]
    fn wave17_gamma_audition_fixture_starts_silent_and_fills_one_scene() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/city/wave17-gamma-audition/fixture.json");
        if !path.exists() {
            return; // private fixture, left out of the public mirror
        }
        let fixture = Fixture::read(&path).unwrap();
        assert_eq!(
            fixture.fixture_id.as_deref(),
            Some("wave17-gamma-audition-chicago-loop-tasting")
        );
        assert_eq!(fixture.sources.len(), fightbox_runtime::MAX_ACTIVE_SOURCES);
        let audition = fixture.audition.as_ref().unwrap();
        assert_eq!(audition.mode, "gamma_audition");
        assert_eq!(audition.macro_ranges_m, [100, 1_000, 10_000]);
        assert_eq!(
            fixture
                .sources
                .iter()
                .filter(|source| source.asset_id.starts_with("squad-"))
                .count(),
            4
        );
        assert!(fixture.sources.iter().all(|source| !source.default_enabled));
        assert!(
            fixture
                .sources
                .iter()
                .all(|source| source.restart_on_enable)
        );
        assert_eq!(
            fixture.initial_listener_position().unwrap(),
            EnuVector3::new(292.5, 292.5, 1.5)
        );
    }

    #[test]
    fn wave17_private_squad_palette_loads_all_prepared_sources_silent() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/city/wave17-squad-palette/fixture.json");
        if !path.exists() {
            return; // private fixture, left out of the public mirror
        }
        let fixture = Fixture::read(&path).unwrap();
        assert_eq!(
            fixture.fixture_id.as_deref(),
            Some("wave17-private-squad-palette")
        );
        assert_eq!(fixture.audition.as_ref().unwrap().mode, "squad_palette");
        assert_eq!(fixture.sources.len(), 15);
        assert!(
            fixture
                .sources
                .iter()
                .all(|source| source.asset_id.starts_with("squad-"))
        );
        assert!(fixture.sources.iter().all(|source| !source.default_enabled));
        assert!(
            fixture
                .sources
                .iter()
                .all(|source| source.restart_on_enable)
        );
    }

    #[test]
    fn checkpoint_fixture_matches_the_approved_scene_contract() {
        let fixture = Fixture::read(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/checkpoint/fixture.json"),
        )
        .unwrap();
        assert_eq!(fixture.fixture_id.as_deref(), Some("checkpoint-block"));
        assert_eq!(fixture.sources.len(), 8);
        assert_eq!(fixture.declared_source_count(), 8);
        assert_eq!(
            fixture.initial_listener_position().unwrap(),
            EnuVector3::new(197.5, 292.5, 1.5)
        );
        let listener_path = fixture.listener.trajectory.as_ref().unwrap();
        assert_eq!(
            listener_path.waypoints_m,
            vec![
                [197.5, 292.5, 1.5],
                [292.5, 292.5, 1.5],
                [292.5, 387.5, 1.5],
                [197.5, 387.5, 1.5],
            ]
        );
        assert_eq!(listener_path.speed_mps, 1.5);
        assert_eq!(listener_path.max_speed_mps, Some(1.5));

        let expected = [
            ("abrams-idle-checkpoint", "squad-abrams-idle", 90.0),
            ("mi8-orbit", "squad-mi8-rotor-close", 126.0),
            ("m2-checkpoint-gun", "squad-m2-burst-loop", 153.0),
            ("dshk-return-fire", "squad-dshk-burst-loop", 154.0),
            ("a10-gunrun-sky", "squad-a10-pass", 127.0),
            ("a10-strike-line", "squad-a10-impacts", 163.0),
            ("a10-gunrun-sky-west", "squad-a10-pass", 127.0),
            ("a10-strike-line-west", "squad-a10-impacts", 163.0),
        ];
        for (source, (id, asset_id, spl)) in fixture.sources.iter().zip(expected) {
            assert_eq!(source.id, id);
            assert_eq!(source.asset_id, asset_id);
            assert_eq!(source.reference_level.db_spl, spl);
        }
        assert!(
            fixture
                .sources
                .iter()
                .enumerate()
                .all(|(index, source)| [2, 3, 5, 7].contains(&index) == source.impulsive)
        );
        assert_eq!(
            fixture.sources[0].extent,
            ExtentDescriptor::LineSegment { length_m: 8.0 }
        );
        assert_eq!(
            fixture.sources[0].initial_position().unwrap(),
            EnuVector3::new(285.0, 305.0, 1.5)
        );
        // Mi-8: 21 m extent models the rotor disc as a 10.5 m occlusion
        // sphere so building shadowing is gradual, not a 1 m point gate.
        assert_eq!(
            fixture.sources[1].extent,
            ExtentDescriptor::LineSegment { length_m: 21.0 }
        );
        for source in &fixture.sources[2..4] {
            assert_eq!(
                source.extent,
                ExtentDescriptor::LineSegment { length_m: 2.0 }
            );
        }
        assert_eq!(
            fixture.sources[2].initial_position().unwrap(),
            EnuVector3::new(289.0, 102.5, 2.0)
        );
        assert_eq!(
            fixture.sources[3].initial_position().unwrap(),
            EnuVector3::new(292.5, 342.5, 2.0)
        );
        for index in [4, 6] {
            assert_eq!(fixture.sources[index].extent, ExtentDescriptor::Point);
            assert_eq!(
                fixture.sources[index].initial_position().unwrap(),
                EnuVector3::new(197.5, 331.167, 127.0)
            );
            assert!(!fixture.sources[index].default_enabled);
        }
        for (index, position) in [
            (5, EnuVector3::new(292.5, 342.5, 2.0)),
            (7, EnuVector3::new(197.5, 342.5, 2.0)),
        ] {
            assert_eq!(
                fixture.sources[index].extent,
                ExtentDescriptor::LineSegment { length_m: 35.0 }
            );
            assert_eq!(fixture.sources[index].initial_position().unwrap(), position);
            assert!(!fixture.sources[index].default_enabled);
        }
        assert_eq!(fixture.sources[4].playback_start_offset_s, 0.0);
        assert_eq!(fixture.sources[5].playback_start_offset_s, 0.0);
        assert_eq!(fixture.sources[6].playback_start_offset_s, 42.0);
        assert_eq!(fixture.sources[7].playback_start_offset_s, 42.0);

        let orbit = fixture.sources[1].trajectory.as_ref().unwrap();
        assert_eq!(orbit.speed_mps, 30.0);
        assert_eq!(orbit.max_speed_mps, Some(30.0));
        assert_eq!(orbit.waypoints_m.len(), 48);
        assert!(orbit.waypoints_m.iter().all(|point| {
            let east = point[0] - 292.5;
            let north = point[1] - 292.5;
            (east.hypot(north) - 190.0).abs() < 1.0e-3 && point[2] == 55.0
        }));

        let east_racetrack = fixture.sources[4].trajectory.as_ref().unwrap();
        let west_racetrack = fixture.sources[6].trajectory.as_ref().unwrap();
        assert_eq!(east_racetrack.waypoints_m, west_racetrack.waypoints_m);
        assert_eq!(east_racetrack.waypoints_m.len(), 36);
        assert_eq!(east_racetrack.speed_mps, 10.452049);
        assert_eq!(east_racetrack.max_speed_mps, Some(10.452049));
    }

    #[test]
    fn fixture_occlusion_samples_drive_point_and_extent_modes() {
        let fixture = Fixture::read(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/city/megablock/fixture.json"),
        )
        .unwrap();
        let config = fixture.simulation_config();
        assert_eq!(config.max_occlusion_samples, 64);
        assert_eq!(
            config.direct_occlusion,
            DirectOcclusionMode::Volumetric {
                radius_m: 1.0,
                sample_count: 64,
            }
        );
        assert_eq!(
            occlusion_mode_for_extent(config, ExtentDescriptor::Point),
            DirectOcclusionMode::Volumetric {
                radius_m: 1.0,
                sample_count: 64,
            }
        );
        assert_eq!(
            occlusion_mode_for_extent(config, ExtentDescriptor::LineSegment { length_m: 6.0 }),
            DirectOcclusionMode::Volumetric {
                radius_m: 3.0,
                sample_count: 64,
            }
        );
        assert_eq!(
            occlusion_mode_for_extent(config, ExtentDescriptor::StereoImage { width_m: 4.0 }),
            DirectOcclusionMode::Volumetric {
                radius_m: 2.0,
                sample_count: 64,
            }
        );
        assert_eq!(
            occlusion_mode_for_extent(config, ExtentDescriptor::MultiPoint { count: 4 }),
            DirectOcclusionMode::Volumetric {
                radius_m: 1.0,
                sample_count: 64,
            }
        );
    }

    #[test]
    fn absent_visibility_range_adopts_two_and_a_half_times_probe_spacing() {
        let fixture = Fixture::read(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/city/megablock/fixture.json"),
        )
        .unwrap();
        let adoption = fixture.visibility_range_adoption();
        assert_eq!(adoption.configured_m, 6.0);
        assert_eq!(adoption.probe_spacing_m, 4.0);
        assert_eq!(adoption.minimum_for_spacing_m, 10.0);
        assert_eq!(adoption.effective_m, 10.0);
        assert!(adoption.rebaselined);
        assert_eq!(fixture.simulation_config().pathing_visibility_range_m, 10.0);
    }

    #[test]
    fn explicit_visibility_range_is_kept_or_adopted_against_the_same_floor() {
        let original = include_str!("../../../fixtures/city/megablock/fixture.json");
        for (configured, expected, rebaselined) in [(12.0, 12.0, false), (5.0, 10.0, true)] {
            let text = original.replace(
                r#""alternate_paths": true,"#,
                &format!(
                    r#""alternate_paths": true,
      "visibility_range_m": {configured},"#
                ),
            );
            let fixture = Fixture::parse(text.as_bytes(), "visibility-test").unwrap();
            let adoption = fixture.visibility_range_adoption();
            assert_eq!(adoption.configured_m, configured as f32);
            assert_eq!(adoption.effective_m, expected);
            assert_eq!(adoption.rebaselined, rebaselined);
        }
    }

    #[test]
    fn direct_flags_default_true_and_explicit_true_preserve_simulation_settings() {
        let original: Value = serde_json::from_str(include_str!(
            "../../../fixtures/city/megablock/fixture.json"
        ))
        .unwrap();
        let mut omitted = original.clone();
        let direct = omitted["simulation"]["direct"].as_object_mut().unwrap();
        direct.remove("distance_attenuation");
        direct.remove("occlusion");
        let defaulted =
            Fixture::parse(&serde_json::to_vec(&omitted).unwrap(), "direct-defaults").unwrap();
        let mut explicit = original;
        explicit["simulation"]["direct"]["distance_attenuation"] = Value::Bool(true);
        explicit["simulation"]["direct"]["occlusion"] = Value::Bool(true);
        let enabled =
            Fixture::parse(&serde_json::to_vec(&explicit).unwrap(), "direct-true").unwrap();
        assert!(
            defaulted.simulation.direct.distance_attenuation
                && defaulted.simulation.direct.occlusion
        );
        assert!(
            enabled.simulation.direct.distance_attenuation && enabled.simulation.direct.occlusion
        );
        assert_eq!(defaulted.simulation_config(), enabled.simulation_config());
    }

    #[test]
    fn direct_flags_reject_unsupported_false_and_malformed_types() {
        let original: Value = serde_json::from_str(include_str!(
            "../../../fixtures/city/megablock/fixture.json"
        ))
        .unwrap();
        for field in ["distance_attenuation", "occlusion"] {
            let mut disabled = original.clone();
            disabled["simulation"]["direct"][field] = Value::Bool(false);
            let error = Fixture::parse(&serde_json::to_vec(&disabled).unwrap(), "direct-false")
                .unwrap_err();
            assert!(
                error.contains(&format!("simulation.direct.{field}=false")),
                "{error}"
            );
            assert!(
                error.contains("not supported by the live Workbench backend"),
                "{error}"
            );
            for malformed in [Value::Null, Value::String("false".into()), Value::from(0)] {
                let mut invalid = original.clone();
                invalid["simulation"]["direct"][field] = malformed;
                let error =
                    Fixture::parse(&serde_json::to_vec(&invalid).unwrap(), "direct-invalid")
                        .unwrap_err();
                assert!(error.contains("expected a boolean"), "{error}");
            }
        }
    }

    #[test]
    fn fixture_rejects_invalid_reference_and_simulation_ranges() {
        let original = include_str!("../../../fixtures/city/megablock/fixture.json");
        for (text, expected) in [
            (
                original.replacen("SplAtOneMeter", "CreativeDb", 1),
                "unknown variant `CreativeDb`",
            ),
            (
                original.replacen("\"occlusion_samples\": 64", "\"occlusion_samples\": 0", 1),
                "occlusion_samples must be in 1..=2147483647",
            ),
            (
                original.replace(
                    r#""alternate_paths": true,"#,
                    r#""alternate_paths": true,
      "visibility_range_m": 0,"#,
                ),
                "visibility_range_m must be finite, positive",
            ),
        ] {
            let error = Fixture::parse(text.as_bytes(), "invalid-range-test").unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?} in fixture error, got: {error}"
            );
        }
    }

    #[test]
    fn gun_street_response_defaults_on_and_can_be_explicitly_dry() {
        let mut wire = serde_json::json!({
            "aim_point_m": [2.0, 0.0, 1.5],
            "muzzle_velocity_mps": 890.0,
            "supersonic_distance_m": 500.0,
            "dispersion_m": 0.0,
            "crack_peak_db_at_30_m": 140.0
        });
        let defaulted: FixtureGunfire = serde_json::from_value(wire.clone()).unwrap();
        assert!(defaulted.street_response);
        wire["street_response"] = false.into();
        let dry: FixtureGunfire = serde_json::from_value(wire).unwrap();
        assert!(!dry.street_response);
    }

    #[test]
    fn source_forward_defaults_north_and_normalizes_valid_heading() {
        let fixture = Fixture::parse(
            include_bytes!("../../../fixtures/city/megablock/fixture.json"),
            "source-heading",
        )
        .unwrap();
        assert_eq!(
            fixture.sources[0].forward_enu_normalized().unwrap(),
            EnuVector3::new(0.0, 1.0, 0.0)
        );
        let mut source = fixture.sources[0].clone();
        source.forward_enu = [3.0, 4.0, 0.0];
        assert_eq!(
            source.forward_enu_normalized().unwrap(),
            EnuVector3::new(0.6, 0.8, 0.0)
        );
    }

    #[test]
    fn source_forward_rejects_vertical_or_zero_heading() {
        let fixture = Fixture::parse(
            include_bytes!("../../../fixtures/city/megablock/fixture.json"),
            "source-heading",
        )
        .unwrap();
        let mut source = fixture.sources[0].clone();
        source.forward_enu = [0.0, 0.0, 1.0];
        assert!(
            source
                .forward_enu_normalized()
                .unwrap_err()
                .contains("horizontal")
        );
        source.forward_enu = [0.0, 0.0, 0.0];
        assert!(
            source
                .forward_enu_normalized()
                .unwrap_err()
                .contains("non-zero")
        );
    }
}
