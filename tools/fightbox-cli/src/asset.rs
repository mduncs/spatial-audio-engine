//! Strict parsing of canonical source-asset truth plus compatibility parsing
//! and mono regeneration for deterministic legacy asset descriptors.
//!
//! A descriptor binds a fixture source to either a deterministic generator or a
//! provenance-pinned WAV. This layer parses it with `deny_unknown_fields`,
//! validates cross-field rules the JSON Schema cannot express, and produces the
//! exact finite mono buffer plus its ebur128-backed analysis. The CLI represents
//! serialized authoring/import truth as [`SourceAssetDescriptor`]. The canonical
//! audio packer accepts mono or stereo PCM WAV through this contract and emits
//! indexed planar chunks for worker-side cache fill. The legacy Phase A loader
//! remains mono-only. Neither path exposes this CLI-private type to the runtime.
//! The scene-owned source drive is derived separately in [`crate::calibrate`].

use std::path::{Path, PathBuf};

use fightbox_evidence::{
    ASSET_ANALYSIS_METHOD_ID, AnalyzedAsset, GeneratedSignal, GeneratorNormalization, SignalError,
    SignalKind, WavSpec, multitone, pink_like, sha256_hex, sine,
};
use serde::{Deserialize, Serialize};

use crate::schema::{ASSET_DESCRIPTOR, SOURCE_ASSET};

const CANONICAL_RATE_HZ: u32 = 48_000;
const CANONICAL_CHUNK_FRAMES: u32 = 48_000;
const CANONICAL_ID_PREFIX: &str = "fightbox.canonical-audio.v1:sha256:";
const MONO_COMPATIBILITY_COMB_LIMIT: f64 = 0.10;
const MONO_COMPATIBILITY_NOTCH_LIMIT_DB: f64 = 6.0;

/// Exact opt-in recipe offered when authored stereo is requested as a Point.
/// Each input f32 is widened to f64. For weights `[w_l, w_r]`, one sample is
/// exactly `w_l.mul_add(f64::from(left), w_r * f64::from(right))` rounded once
/// to f32. The unit-norm weights make this an energy-normalized basis
/// projection, and the resulting mono bytes receive their own artifact ID.
pub const PCA_CENTER_MONO_DERIVATIVE_RECIPE_ID: &str =
    "fightbox.pca-center-mono.v1|f64-wl-mul-add-wr-product|round-f32";

/// Frozen synthetic-width recipe identified by `PresentationDerivation`.
///
/// The canonical asset remains mono. On worker-side reads the presentation
/// adapter derives an anti-symmetric side signal from two fixed delayed taps,
/// then emits `left = center + side` and `right = center - side`. The exact
/// recipe hash is serialized in the source descriptor, making it impossible to
/// mistake this synthetic presentation for authored or recovered stereo.
pub const MONO_EXPANSION_RECIPE_ID: &str = "fightbox.mono-expanded.v1";
pub const MONO_EXPANSION_NEAR_DELAY_FRAMES: usize = 257;
pub const MONO_EXPANSION_FAR_DELAY_FRAMES: usize = 1_103;
pub const MONO_EXPANSION_SIDE_GAIN_NUMERATOR: i32 = 3;
pub const MONO_EXPANSION_SIDE_GAIN_DENOMINATOR: i32 = 16;
pub const MONO_EXPANSION_RECIPE_TEXT: &str = concat!(
    "fightbox.mono-expanded.v1|rate=48000|",
    "side=(mono[n-257]-mono[n-1103])*3/16|",
    "planes=left:center+side,right:center-side|",
    "collapse=bit-exact-f32-and-f64-average|",
    "rounding=round-f32-with-side-halving"
);

/// Hash recorded by a `mono_expanded` source descriptor.
#[must_use]
pub fn mono_expansion_recipe_sha256() -> String {
    sha256_hex(MONO_EXPANSION_RECIPE_TEXT.as_bytes())
}

/// Channel layout of the decoded source program. This is independent from the
/// scene geometry and from presentation provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetLayout {
    Mono,
    #[serde(rename = "stereo_lr")]
    StereoLR,
}

impl AssetLayout {
    #[must_use]
    pub const fn channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::StereoLR => 2,
        }
    }
}

/// How the presentation represented by an asset came into being.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationProvenance {
    NativeMono,
    AuthoredStereo,
    MonoExpanded,
}

/// Scene geometry whose compatibility is declared by the source contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceGeometry {
    Point,
    MultiPoint,
    LineSegment,
    StereoImage,
}

/// Presentation shape admitted by validated source truth before runtime block
/// construction. Layout, geometry, and provenance remain separate axes even
/// though the supported combinations are represented explicitly here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourcePresentation {
    NativeMono { geometry: SourceGeometry },
    AuthoredStereoImage,
    MonoExpandedStereoImage,
}

impl SourcePresentation {
    #[must_use]
    pub const fn program_plane_count(self) -> usize {
        match self {
            Self::NativeMono { .. } => 1,
            Self::AuthoredStereoImage | Self::MonoExpandedStereoImage => 2,
        }
    }

    #[must_use]
    pub const fn provenance(self) -> PresentationProvenance {
        match self {
            Self::NativeMono { .. } => PresentationProvenance::NativeMono,
            Self::AuthoredStereoImage => PresentationProvenance::AuthoredStereo,
            Self::MonoExpandedStereoImage => PresentationProvenance::MonoExpanded,
        }
    }
}

/// Explicit alternative to an unsupported authored-stereo Point request.
/// Constructing or applying this offer is separate from admission, so the
/// runtime adapter can never discard a channel as an implicit fallback.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PcaCenterMonoDerivativeOffer {
    pub source_artifact_id: String,
    pub recipe_id: &'static str,
    pub center_weights_lr: [f64; 2],
}

impl PcaCenterMonoDerivativeOffer {
    /// Apply the offered unit-energy PCA projection. This helper is deliberately
    /// opt-in and does not mutate or replace the authored-stereo source asset.
    pub fn derive_planar_f32(&self, left: &[f32], right: &[f32]) -> Result<Vec<f32>, String> {
        if left.len() != right.len() || left.is_empty() {
            return Err(
                "PCA-center mono derivation requires non-empty, equal-length L/R planes".into(),
            );
        }
        let [left_weight, right_weight] = self.center_weights_lr;
        let mut center = Vec::with_capacity(left.len());
        for (&left, &right) in left.iter().zip(right) {
            if !left.is_finite() || !right.is_finite() {
                return Err("PCA-center mono derivation requires finite L/R samples".into());
            }
            let sample = left_weight.mul_add(f64::from(left), right_weight * f64::from(right));
            let sample = sample as f32;
            if !sample.is_finite() {
                return Err("PCA-center mono derivation produced a non-finite sample".into());
            }
            center.push(sample);
        }
        Ok(center)
    }
}

/// Deterministic presentation-admission failures. In particular, stereo Point
/// rejection carries a machine-readable derivative offer instead of silently
/// narrowing two planes to one.
#[derive(Clone, Debug, PartialEq)]
pub enum PresentationAdmissionError {
    InvalidSourceContract(String),
    StereoPointRequiresExplicitMonoDerivative(PcaCenterMonoDerivativeOffer),
    StereoLineSegmentDeferred,
    IncompatibleAxes {
        layout: AssetLayout,
        provenance: PresentationProvenance,
        geometry: SourceGeometry,
    },
}

impl std::fmt::Display for PresentationAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSourceContract(error) => {
                write!(formatter, "invalid source contract: {error}")
            }
            Self::StereoPointRequiresExplicitMonoDerivative(offer) => write!(
                formatter,
                "stereo_lr + point is unsupported; explicitly create a distinct deterministic PCA-center mono derivative with recipe {} from {}",
                offer.recipe_id, offer.source_artifact_id
            ),
            Self::StereoLineSegmentDeferred => {
                formatter.write_str("stereo_lr + line_segment is deferred in V1")
            }
            Self::IncompatibleAxes {
                layout,
                provenance,
                geometry,
            } => write!(
                formatter,
                "geometry {geometry:?} is incompatible with layout {layout:?} and provenance {provenance:?}"
            ),
        }
    }
}

impl std::error::Error for PresentationAdmissionError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaContainer {
    DeterministicGenerator,
    Wav,
    Aiff,
    Caf,
    Flac,
    M4a,
    Mp3,
    FightboxPlanarChunksV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    SineGenerator,
    MultitoneGenerator,
    PinkLikeGenerator,
    PcmInteger,
    PcmFloat,
    Flac,
    Aac,
    Mp3,
    PcmF32PlanarLe,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaFormat {
    pub container: MediaContainer,
    pub codec: AudioCodec,
    pub sample_rate_hz: u32,
    pub layout: AssetLayout,
    pub lossy: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalMedia {
    pub content_sha256: String,
    pub format: MediaFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalChunkStorage {
    RawOrZstdIfSmaller,
}

/// Immutable identity of deterministic 48 kHz planar f32 PCM. Indexed package
/// and cache files cannot change this decoded-content identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalArtifact {
    pub artifact_id: String,
    pub canonical_pcm_sha256: String,
    pub format: MediaFormat,
    pub frame_count: u64,
    pub chunk_frames: u32,
    pub chunk_storage: CanonicalChunkStorage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedTool {
    pub implementation: String,
    pub revision: String,
    pub settings_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalizationProvenance {
    pub decoder: PinnedTool,
    pub resampler: PinnedTool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLabel {
    Mono,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LevelMeasurements {
    pub rms_dbfs: f64,
    pub true_peak_dbtp: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelMeasurements {
    pub channel: ChannelLabel,
    pub levels: LevelMeasurements,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcaAnalysis {
    pub center_weights_lr: [f64; 2],
    pub width_weights_lr: [f64; 2],
    pub center_energy: f64,
    pub width_energy: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonoCompatibilityStatus {
    Compatible,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonoCompatibilityAnalysis {
    pub status: MonoCompatibilityStatus,
    pub score: f64,
    pub short_lag_comb_correlation_delta: f64,
    pub regular_notch_depth_delta_db: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StereoMeasurements {
    pub pca: PcaAnalysis,
    pub correlation: f64,
    pub mono_compatibility: MonoCompatibilityAnalysis,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAssetMeasurements {
    pub analysis_revision: String,
    pub per_channel: Vec<ChannelMeasurements>,
    pub aggregate: LevelMeasurements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stereo: Option<StereoMeasurements>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionEvidence {
    AuthoredDry,
    RecordedMotion,
    LegacyUnspecified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MotionProvenance {
    pub recording_carries_motion: bool,
    pub evidence: MotionEvidence,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightsStatus {
    Generated,
    PublicDomain,
    Licensed,
    Restricted,
    LegacyUnverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RightsProvenance {
    pub status: RightsStatus,
    pub license: String,
    pub evidence: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Seekability {
    SampleAccurate,
    DeterministicGenerator,
    NotSeekable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationDerivation {
    pub source_artifact_id: String,
    pub recipe_sha256: String,
}

/// The CLI's one serialized authoring/import truth. Legacy asset-descriptor v1
/// JSON is normalized into this same type. Runtime code consumes planar channel
/// blocks emitted from this contract, not this CLI-private Rust type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAssetDescriptor {
    pub schema_version: String,
    pub asset_id: String,
    pub layout: AssetLayout,
    pub presentation_provenance: PresentationProvenance,
    pub compatible_geometries: Vec<SourceGeometry>,
    pub original: OriginalMedia,
    pub canonical: CanonicalArtifact,
    pub canonicalization: CanonicalizationProvenance,
    pub measurements: SourceAssetMeasurements,
    pub motion: MotionProvenance,
    pub rights: RightsProvenance,
    pub seekability: Seekability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivation: Option<PresentationDerivation>,
}

impl SourceAssetDescriptor {
    /// Parse either the canonical source contract or a legacy mono descriptor.
    /// Legacy input is regenerated and measured so callers always receive this
    /// one normalized type; whitespace in the legacy JSON cannot change its
    /// canonical identity.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("invalid source asset JSON ({e})"))?;
        let schema = value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "source asset JSON is missing string schema_version".to_string())?;
        if schema == ASSET_DESCRIPTOR {
            return AssetDescriptor::parse(text)?.normalize_source_contract();
        }
        if schema != SOURCE_ASSET {
            return Err(format!(
                "unsupported source asset schema_version {schema}; expected {SOURCE_ASSET} or legacy {ASSET_DESCRIPTOR}"
            ));
        }
        let descriptor: Self =
            serde_json::from_str(text).map_err(|e| format!("invalid source asset JSON ({e})"))?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SOURCE_ASSET {
            return Err(format!(
                "schema_version must be {SOURCE_ASSET}, got {}",
                self.schema_version
            ));
        }
        validate_asset_id(&self.asset_id)?;
        match (self.layout, self.presentation_provenance) {
            (AssetLayout::Mono, PresentationProvenance::NativeMono)
            | (AssetLayout::Mono, PresentationProvenance::MonoExpanded)
            | (AssetLayout::StereoLR, PresentationProvenance::AuthoredStereo) => {}
            (AssetLayout::Mono, PresentationProvenance::AuthoredStereo) => {
                return Err("authored_stereo requires layout stereo_lr".into());
            }
            (AssetLayout::StereoLR, PresentationProvenance::NativeMono) => {
                return Err("native_mono requires layout mono".into());
            }
            (AssetLayout::StereoLR, PresentationProvenance::MonoExpanded) => {
                return Err(
                    "mono_expanded is a derived mono presentation and requires layout mono".into(),
                );
            }
        }
        self.original.validate("original")?;
        self.canonical.validate(self.layout)?;
        self.canonicalization.validate()?;
        self.measurements.validate(self.layout)?;
        self.motion.validate()?;
        self.rights.validate()?;
        if self.seekability == Seekability::NotSeekable {
            return Err("canonical source assets must be seekable; microphone/network/live inputs are unsupported".into());
        }
        if self.original.format.layout != self.layout {
            return Err("original format layout must match descriptor layout".into());
        }
        validate_geometry_set(
            self.layout,
            self.presentation_provenance,
            &self.compatible_geometries,
        )?;
        match self.presentation_provenance {
            PresentationProvenance::MonoExpanded => {
                let derivation = self.derivation.as_ref().ok_or_else(|| {
                    "mono_expanded requires derivation source_artifact_id and recipe_sha256"
                        .to_string()
                })?;
                validate_artifact_id(&derivation.source_artifact_id)?;
                validate_sha256("derivation.recipe_sha256", &derivation.recipe_sha256)?;
                if derivation.source_artifact_id != self.canonical.artifact_id {
                    return Err(
                        "mono_expanded derivation.source_artifact_id must identify the unchanged canonical mono PCM"
                            .into(),
                    );
                }
                let expected_recipe = mono_expansion_recipe_sha256();
                if derivation.recipe_sha256 != expected_recipe {
                    return Err(format!(
                        "mono_expanded derivation.recipe_sha256 must identify frozen recipe {MONO_EXPANSION_RECIPE_ID} ({expected_recipe})"
                    ));
                }
            }
            _ if self.derivation.is_some() => {
                return Err("derivation is only valid for mono_expanded provenance".into());
            }
            _ => {}
        }
        if self.layout == AssetLayout::StereoLR {
            let stereo = self
                .measurements
                .stereo
                .expect("validated stereo measurements");
            if stereo.mono_compatibility.status != MonoCompatibilityStatus::Compatible {
                return Err(
                    "authored stereo is rejected: mono_compatibility.status must be compatible"
                        .into(),
                );
            }
            if stereo.mono_compatibility.short_lag_comb_correlation_delta
                > MONO_COMPATIBILITY_COMB_LIMIT
            {
                return Err(format!(
                    "authored stereo is mono-incompatible: short-lag comb correlation delta exceeds {MONO_COMPATIBILITY_COMB_LIMIT}"
                ));
            }
            if stereo.mono_compatibility.regular_notch_depth_delta_db
                > MONO_COMPATIBILITY_NOTCH_LIMIT_DB
            {
                return Err(format!(
                    "authored stereo is mono-incompatible: regular-notch depth delta exceeds {MONO_COMPATIBILITY_NOTCH_LIMIT_DB} dB"
                ));
            }
        }
        Ok(())
    }

    /// Admit one presentation without collapsing the independent source axes.
    /// Authored stereo requested as a Point is rejected with a deterministic,
    /// explicit PCA-center derivative offer; it is never narrowed in place.
    pub fn admit_presentation(
        &self,
        geometry: SourceGeometry,
    ) -> Result<SourcePresentation, PresentationAdmissionError> {
        self.validate()
            .map_err(PresentationAdmissionError::InvalidSourceContract)?;
        match (self.layout, self.presentation_provenance, geometry) {
            (
                AssetLayout::Mono,
                PresentationProvenance::NativeMono,
                SourceGeometry::Point | SourceGeometry::MultiPoint | SourceGeometry::LineSegment,
            ) => Ok(SourcePresentation::NativeMono { geometry }),
            (
                AssetLayout::StereoLR,
                PresentationProvenance::AuthoredStereo,
                SourceGeometry::StereoImage,
            ) => Ok(SourcePresentation::AuthoredStereoImage),
            (
                AssetLayout::Mono,
                PresentationProvenance::MonoExpanded,
                SourceGeometry::StereoImage,
            ) => Ok(SourcePresentation::MonoExpandedStereoImage),
            (AssetLayout::StereoLR, _, SourceGeometry::Point) => {
                let stereo = self
                    .measurements
                    .stereo
                    .expect("validated stereo source has PCA measurements");
                Err(
                    PresentationAdmissionError::StereoPointRequiresExplicitMonoDerivative(
                        PcaCenterMonoDerivativeOffer {
                            source_artifact_id: self.canonical.artifact_id.clone(),
                            recipe_id: PCA_CENTER_MONO_DERIVATIVE_RECIPE_ID,
                            center_weights_lr: stereo.pca.center_weights_lr,
                        },
                    ),
                )
            }
            (AssetLayout::StereoLR, _, SourceGeometry::LineSegment) => {
                Err(PresentationAdmissionError::StereoLineSegmentDeferred)
            }
            _ => Err(PresentationAdmissionError::IncompatibleAxes {
                layout: self.layout,
                provenance: self.presentation_provenance,
                geometry,
            }),
        }
    }

    /// Validate a scene geometry against the frozen V1 compatibility table.
    pub fn validate_geometry(&self, geometry: SourceGeometry) -> Result<(), String> {
        self.admit_presentation(geometry)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Recordings that already contain motion can only be presented through a
    /// static proxy until dry material is reauthored, preventing double Doppler.
    pub fn validate_motion_use(&self, scene_source_moves: bool) -> Result<(), String> {
        if scene_source_moves && self.motion.recording_carries_motion {
            return Err("recording_carries_motion=true requires a static-proxy presentation; moving it would apply motion twice".into());
        }
        if scene_source_moves && self.motion.evidence == MotionEvidence::LegacyUnspecified {
            return Err("legacy recording has unspecified motion provenance; author recording_carries_motion before admitting scene motion".into());
        }
        Ok(())
    }
}

impl OriginalMedia {
    fn validate(&self, path: &str) -> Result<(), String> {
        validate_sha256(&format!("{path}.content_sha256"), &self.content_sha256)?;
        self.format.validate_original(path)
    }
}

impl MediaFormat {
    fn validate_original(&self, path: &str) -> Result<(), String> {
        if self.sample_rate_hz == 0 {
            return Err(format!("{path}.format.sample_rate_hz must be positive"));
        }
        let supported = matches!(
            (self.container, self.codec, self.lossy),
            (
                MediaContainer::DeterministicGenerator,
                AudioCodec::SineGenerator
                    | AudioCodec::MultitoneGenerator
                    | AudioCodec::PinkLikeGenerator,
                false
            ) | (
                MediaContainer::Wav | MediaContainer::Aiff | MediaContainer::Caf,
                AudioCodec::PcmInteger | AudioCodec::PcmFloat,
                false
            ) | (MediaContainer::Flac, AudioCodec::Flac, false)
                | (MediaContainer::M4a, AudioCodec::Aac, true)
                | (MediaContainer::Mp3, AudioCodec::Mp3, true)
        );
        if !supported {
            return Err(format!(
                "{path}.format has unsupported container/codec/lossy combination: {:?}/{:?}/{}",
                self.container, self.codec, self.lossy
            ));
        }
        Ok(())
    }
}

impl CanonicalArtifact {
    fn validate(&self, layout: AssetLayout) -> Result<(), String> {
        validate_sha256("canonical.canonical_pcm_sha256", &self.canonical_pcm_sha256)?;
        validate_artifact_id(&self.artifact_id)?;
        let expected_id = format!("{CANONICAL_ID_PREFIX}{}", self.canonical_pcm_sha256);
        if self.artifact_id != expected_id {
            return Err(format!(
                "canonical.artifact_id must be the immutable PCM identity {expected_id}"
            ));
        }
        if self.format.container != MediaContainer::FightboxPlanarChunksV1
            || self.format.codec != AudioCodec::PcmF32PlanarLe
            || self.format.sample_rate_hz != CANONICAL_RATE_HZ
            || self.format.layout != layout
            || self.format.lossy
        {
            return Err(
                "canonical.format must be lossless fightbox_planar_chunks_v1/pcm_f32_planar_le at 48000 Hz with the descriptor layout"
                    .into(),
            );
        }
        if self.frame_count == 0 {
            return Err("canonical.frame_count must be positive".into());
        }
        if self.chunk_frames != CANONICAL_CHUNK_FRAMES {
            return Err(format!(
                "canonical.chunk_frames must be {CANONICAL_CHUNK_FRAMES} (one second at 48 kHz)"
            ));
        }
        Ok(())
    }
}

impl PinnedTool {
    fn validate(&self, path: &str) -> Result<(), String> {
        if self.implementation.trim().is_empty() || self.revision.trim().is_empty() {
            return Err(format!(
                "{path}.implementation and {path}.revision must be non-empty pinned identities"
            ));
        }
        validate_sha256(&format!("{path}.settings_sha256"), &self.settings_sha256)
    }
}

impl CanonicalizationProvenance {
    fn validate(&self) -> Result<(), String> {
        self.decoder.validate("canonicalization.decoder")?;
        self.resampler.validate("canonicalization.resampler")
    }
}

impl LevelMeasurements {
    fn validate(self, path: &str) -> Result<(), String> {
        if !self.rms_dbfs.is_finite() || !self.true_peak_dbtp.is_finite() {
            return Err(format!("{path} levels must be finite"));
        }
        if self.rms_dbfs > self.true_peak_dbtp {
            return Err(format!(
                "{path}.rms_dbfs must not exceed {path}.true_peak_dbtp"
            ));
        }
        Ok(())
    }
}

impl PcaAnalysis {
    fn validate(self) -> Result<(), String> {
        let [c0, c1] = self.center_weights_lr;
        let [w0, w1] = self.width_weights_lr;
        if ![c0, c1, w0, w1, self.center_energy, self.width_energy]
            .iter()
            .all(|value| value.is_finite())
        {
            return Err("measurements.stereo.pca values must be finite".into());
        }
        let center_norm = c0.mul_add(c0, c1 * c1);
        let width_norm = w0.mul_add(w0, w1 * w1);
        let dot = c0.mul_add(w0, c1 * w1);
        if (center_norm - 1.0).abs() > 1e-6 || (width_norm - 1.0).abs() > 1e-6 || dot.abs() > 1e-6 {
            return Err("measurements.stereo.pca vectors must be orthonormal".into());
        }
        if c0 < 0.0 || (c0 == 0.0 && c1 < 0.0) {
            return Err("measurements.stereo.pca center sign must use the deterministic first-nonzero-positive convention".into());
        }
        if (w0 + c1).abs() > 1e-6 || (w1 - c0).abs() > 1e-6 {
            return Err(
                "measurements.stereo.pca width vector must be [-center_r, center_l]".into(),
            );
        }
        if self.width_energy < 0.0 || self.center_energy < self.width_energy {
            return Err(
                "measurements.stereo.pca requires center_energy >= width_energy >= 0".into(),
            );
        }
        Ok(())
    }
}

impl SourceAssetMeasurements {
    fn validate(&self, layout: AssetLayout) -> Result<(), String> {
        if self.analysis_revision.trim().is_empty() {
            return Err("measurements.analysis_revision must be non-empty".into());
        }
        let expected_labels: &[ChannelLabel] = match layout {
            AssetLayout::Mono => &[ChannelLabel::Mono],
            AssetLayout::StereoLR => &[ChannelLabel::Left, ChannelLabel::Right],
        };
        let actual_labels: Vec<_> = self.per_channel.iter().map(|item| item.channel).collect();
        if actual_labels != expected_labels {
            return Err(format!(
                "measurements.per_channel labels must be {expected_labels:?} for layout {layout:?}"
            ));
        }
        for (index, channel) in self.per_channel.iter().enumerate() {
            channel
                .levels
                .validate(&format!("measurements.per_channel[{index}]"))?;
        }
        self.aggregate.validate("measurements.aggregate")?;
        let mean_square = self
            .per_channel
            .iter()
            .map(|channel| 10.0_f64.powf(channel.levels.rms_dbfs / 10.0))
            .sum::<f64>()
            / layout.channels() as f64;
        let expected_rms = 10.0 * mean_square.log10();
        if (self.aggregate.rms_dbfs - expected_rms).abs() > 1e-3 {
            return Err(format!(
                "measurements.aggregate.rms_dbfs must be the per-channel energy aggregate ({expected_rms})"
            ));
        }
        let expected_peak = self
            .per_channel
            .iter()
            .map(|channel| channel.levels.true_peak_dbtp)
            .fold(f64::NEG_INFINITY, f64::max);
        if (self.aggregate.true_peak_dbtp - expected_peak).abs() > 1e-3 {
            return Err(format!(
                "measurements.aggregate.true_peak_dbtp must be the per-channel maximum ({expected_peak})"
            ));
        }
        match (layout, self.stereo) {
            (AssetLayout::Mono, None) => {}
            (AssetLayout::Mono, Some(_)) => {
                return Err("mono measurements must not carry stereo PCA/correlation data".into());
            }
            (AssetLayout::StereoLR, None) => {
                return Err(
                    "stereo_lr measurements require PCA, correlation, and mono compatibility"
                        .into(),
                );
            }
            (AssetLayout::StereoLR, Some(stereo)) => {
                stereo.pca.validate()?;
                if !stereo.correlation.is_finite() || !(-1.0..=1.0).contains(&stereo.correlation) {
                    return Err(
                        "measurements.stereo.correlation must be finite and in [-1, 1]".into(),
                    );
                }
                let compatibility = stereo.mono_compatibility;
                if !compatibility.score.is_finite()
                    || !(0.0..=1.0).contains(&compatibility.score)
                    || !compatibility.short_lag_comb_correlation_delta.is_finite()
                    || compatibility.short_lag_comb_correlation_delta < 0.0
                    || !compatibility.regular_notch_depth_delta_db.is_finite()
                    || compatibility.regular_notch_depth_delta_db < 0.0
                {
                    return Err("measurements.stereo.mono_compatibility metrics must be finite and non-negative, with score in [0, 1]".into());
                }
            }
        }
        Ok(())
    }
}

impl MotionProvenance {
    fn validate(&self) -> Result<(), String> {
        if self.description.trim().is_empty() {
            return Err("motion.description must not be empty".into());
        }
        match (self.recording_carries_motion, self.evidence) {
            (true, MotionEvidence::RecordedMotion)
            | (false, MotionEvidence::AuthoredDry)
            | (false, MotionEvidence::LegacyUnspecified) => Ok(()),
            _ => Err("motion.recording_carries_motion conflicts with motion.evidence".into()),
        }
    }
}

impl RightsProvenance {
    fn validate(&self) -> Result<(), String> {
        if self.license.trim().is_empty() || self.evidence.trim().is_empty() {
            return Err("rights.license and rights.evidence must not be empty".into());
        }
        Ok(())
    }
}

fn validate_asset_id(asset_id: &str) -> Result<(), String> {
    if asset_id.is_empty()
        || !asset_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !asset_id
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err("asset_id must match ^[a-z0-9][a-z0-9-]*$".into());
    }
    Ok(())
}

fn validate_sha256(path: &str, value: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{path} must be 64 lowercase hexadecimal characters"
        ));
    }
    Ok(())
}

fn validate_artifact_id(value: &str) -> Result<(), String> {
    let Some(hash) = value.strip_prefix(CANONICAL_ID_PREFIX) else {
        return Err(format!(
            "canonical artifact identity must begin with {CANONICAL_ID_PREFIX}"
        ));
    };
    validate_sha256("canonical artifact identity hash", hash)
}

fn validate_geometry_set(
    layout: AssetLayout,
    provenance: PresentationProvenance,
    actual: &[SourceGeometry],
) -> Result<(), String> {
    let expected: &[SourceGeometry] = match (layout, provenance) {
        (AssetLayout::Mono, PresentationProvenance::NativeMono) => &[
            SourceGeometry::Point,
            SourceGeometry::MultiPoint,
            SourceGeometry::LineSegment,
        ],
        (AssetLayout::Mono, PresentationProvenance::MonoExpanded)
        | (AssetLayout::StereoLR, PresentationProvenance::AuthoredStereo) => {
            &[SourceGeometry::StereoImage]
        }
        _ => &[],
    };
    if actual != expected {
        return Err(format!(
            "compatible_geometries must be exactly {expected:?} for layout {layout:?} and provenance {provenance:?}; unsupported combinations are not silently admitted"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Sine,
    Multitone,
    PinkLike,
    Wav,
}

impl AssetKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sine => "sine",
            Self::Multitone => "multitone",
            Self::PinkLike => "pink_like",
            Self::Wav => "wav",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SineBlock {
    pub frequency_hz: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultitoneBlock {
    pub frequencies_hz: Vec<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinkLikeBlock {
    pub seed: u64,
}

/// A file-backed mono program source.
///
/// The file hash is mandatory provenance. Relative paths are resolved from the
/// repository root; absolute paths allow intentionally uncommitted recordings.
/// The decoded PCM is normalized to `target_rms_dbfs` using the same dBFS
/// convention as generated assets: a full-scale-peak sine is approximately
/// -3.0103 dBFS RMS.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WavBlock {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub start_frame: u64,
    #[serde(default)]
    pub r#loop: bool,
}

/// Generator block. Exactly one inner block is permitted; the kind/block match
/// is enforced after deserialization because JSON Schema cannot express it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default)]
    pub sine: Option<SineBlock>,
    #[serde(default)]
    pub multitone: Option<MultitoneBlock>,
    #[serde(default)]
    pub pink_like: Option<PinkLikeBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wav: Option<WavBlock>,
}

/// The parsed deterministic asset descriptor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetDescriptor {
    pub schema_version: String,
    pub asset_id: String,
    pub kind: AssetKind,
    pub generator: Generator,
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub duration_s: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub onsets_s: Vec<f64>,
    pub target_rms_dbfs: f64,
    #[serde(default)]
    pub expected_reference_rms_dbfs: Option<f64>,
    #[allow(dead_code)]
    pub calibration: Calibration,
    pub non_claims: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    #[serde(default)]
    pub applied_gain_db: Option<f64>,
}

/// A descriptor whose kind/generator contract has been validated and whose frame
/// count has been computed.
#[derive(Clone, Debug)]
pub struct ResolvedAsset {
    pub descriptor: AssetDescriptor,
    pub frame_count: usize,
}

impl AssetDescriptor {
    /// Parse and structurally validate a descriptor from its JSON text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("invalid asset JSON ({e})"))?;
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            == Some(SOURCE_ASSET)
        {
            SourceAssetDescriptor::parse(text)?;
            return Err(
                "fightbox.source-asset.v1 is consumed through the canonical package/runtime adapter; the legacy Phase A loader accepts fightbox.asset-descriptor.v1 mono PCM only"
                    .into(),
            );
        }
        let descriptor: AssetDescriptor =
            serde_json::from_str(text).map_err(|e| format!("invalid asset JSON ({e})"))?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Preserve the legacy loader's explicit mono presentation contract while
    /// canonical packages gain channel-aware runtime blocks. This performs no
    /// media decode and is therefore suitable for fixture-level admission.
    pub fn admit_legacy_presentation(
        &self,
        geometry: SourceGeometry,
    ) -> Result<SourcePresentation, String> {
        self.validate()?;
        if self.channels != 1 {
            return Err(
                "legacy Phase A runtime presentation accepts mono assets only; package stereo through fightbox.source-asset.v1"
                    .into(),
            );
        }
        match geometry {
            SourceGeometry::Point | SourceGeometry::MultiPoint | SourceGeometry::LineSegment => {
                Ok(SourcePresentation::NativeMono { geometry })
            }
            SourceGeometry::StereoImage => Err(
                "legacy native_mono assets do not implicitly expand to StereoImage presentation"
                    .into(),
            ),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != ASSET_DESCRIPTOR {
            return Err(format!(
                "schema_version must be {ASSET_DESCRIPTOR}, got {}",
                self.schema_version
            ));
        }
        if self.channels != 1 && self.channels != 2 {
            return Err("channels must be 1 or 2".into());
        }
        if self.sample_rate_hz == 0 {
            return Err("sample_rate_hz must be positive".into());
        }
        if !self.duration_s.is_finite() || self.duration_s <= 0.0 {
            return Err("duration_s must be finite and positive".into());
        }
        let mut previous_onset = None;
        for &onset in &self.onsets_s {
            if !onset.is_finite() || onset < 0.0 || onset >= self.duration_s {
                return Err("onsets_s values must be finite and in [0, duration_s)".into());
            }
            if previous_onset.is_some_and(|previous| onset <= previous) {
                return Err("onsets_s must be strictly ascending".into());
            }
            previous_onset = Some(onset);
        }
        if !self.target_rms_dbfs.is_finite() || self.target_rms_dbfs >= 0.0 {
            return Err("target_rms_dbfs must be finite and strictly below 0 dBFS".into());
        }
        // The selected kind must carry exactly its matching generator block.
        let present: Vec<&str> = [
            self.generator.sine.is_some().then_some("sine"),
            self.generator.multitone.is_some().then_some("multitone"),
            self.generator.pink_like.is_some().then_some("pink_like"),
            self.generator.wav.is_some().then_some("wav"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if present != [self.kind.as_str()] {
            return Err(format!(
                "kind {} requires exactly the generator.{} block; found {present:?}",
                self.kind.as_str(),
                self.kind.as_str()
            ));
        }
        let nyquist = self.sample_rate_hz as f64 / 2.0;
        match self.kind {
            AssetKind::Sine => {
                self.require_signal_module()?;
                let freq = self.generator.sine.unwrap().frequency_hz;
                check_frequency(freq, nyquist)?;
            }
            AssetKind::Multitone => {
                self.require_signal_module()?;
                let freqs = &self.generator.multitone.as_ref().unwrap().frequencies_hz;
                if freqs.is_empty() {
                    return Err("multitone requires at least one frequency".into());
                }
                let mut seen = std::collections::HashSet::new();
                for &freq in freqs {
                    check_frequency(freq, nyquist)?;
                    if !seen.insert(freq.to_bits()) {
                        return Err(format!("multitone frequency {freq} repeats"));
                    }
                }
            }
            AssetKind::PinkLike => {
                self.require_signal_module()?;
                let _ = self.generator.pink_like.unwrap().seed;
            }
            AssetKind::Wav => {
                if self.generator.module.is_some() {
                    return Err("kind wav does not use generator.module".into());
                }
                if self.channels != 1 {
                    return Err("wav asset descriptors must declare channels=1".into());
                }
                if self.sample_rate_hz != 48_000 {
                    return Err("wav asset descriptors must declare sample_rate_hz=48000".into());
                }
                let wav = self.generator.wav.as_ref().unwrap();
                if wav.path.is_empty() {
                    return Err("generator.wav.path must not be empty".into());
                }
                if wav.sha256.len() != 64
                    || !wav
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(
                        "generator.wav.sha256 must be 64 lowercase hexadecimal characters".into(),
                    );
                }
            }
        }
        if !self.non_claims.iter().any(|c| {
            c == "This descriptor makes no delivered-ear-SPL claim without output calibration."
        }) {
            return Err("asset descriptor must carry the no-delivered-ear-SPL non-claim".into());
        }
        Ok(())
    }

    fn require_signal_module(&self) -> Result<(), String> {
        if self.generator.module.as_deref() != Some("fightbox_evidence::signal") {
            return Err("generator.module must be fightbox_evidence::signal".into());
        }
        Ok(())
    }

    /// Resolve this descriptor to a generator-ready asset, computing frame count.
    pub fn resolve(&self) -> Result<ResolvedAsset, String> {
        let frame_count = (self.duration_s * self.sample_rate_hz as f64).round() as usize;
        if frame_count == 0 {
            return Err("duration_s rounded to zero frames".into());
        }
        Ok(ResolvedAsset {
            descriptor: self.clone(),
            frame_count,
        })
    }

    /// Converts descriptor-authored loop onsets to exact sample-frame indices.
    pub fn onset_frames(&self) -> Result<Vec<u32>, String> {
        let frame_count = (self.duration_s * self.sample_rate_hz as f64).round() as u64;
        let mut frames = Vec::with_capacity(self.onsets_s.len());
        for &onset in &self.onsets_s {
            let frame = (onset * self.sample_rate_hz as f64).round() as u64;
            if frame >= frame_count || frame > u64::from(u32::MAX) {
                return Err("onsets_s value does not map inside the loop frame range".into());
            }
            let frame = frame as u32;
            if frames.last().is_some_and(|previous| frame <= *previous) {
                return Err("onsets_s values must map to distinct ascending frames".into());
            }
            frames.push(frame);
        }
        Ok(frames)
    }

    /// Deterministically normalize a legacy descriptor into the canonical
    /// source-asset contract. Playback still consumes the legacy mono signal;
    /// this produces the immutable truth object for the future seam.
    pub fn normalize_source_contract(&self) -> Result<SourceAssetDescriptor, String> {
        self.validate()?;
        if self.channels != 1 {
            return Err(
                "legacy asset-descriptor normalization supports mono only; author stereo directly as fightbox.source-asset.v1"
                    .into(),
            );
        }
        if self.sample_rate_hz != CANONICAL_RATE_HZ {
            return Err(format!(
                "legacy asset-descriptor normalization requires 48000 Hz; {} Hz needs the pinned Wave 1 resampler",
                self.sample_rate_hz
            ));
        }
        let resolved = self.resolve()?;
        let (signal, analyzed) = resolved.regenerate_mono()?;
        let canonical_pcm_sha256 = hash_planar_f32(&signal.samples);
        let descriptor_bytes = serde_json::to_vec(self)
            .map_err(|error| format!("cannot canonicalize legacy descriptor JSON: {error}"))?;
        let descriptor_sha256 = sha256_hex(&descriptor_bytes);
        let (original, decoder, motion, rights, seekability) = match self.kind {
            AssetKind::Sine | AssetKind::Multitone | AssetKind::PinkLike => {
                let codec = match self.kind {
                    AssetKind::Sine => AudioCodec::SineGenerator,
                    AssetKind::Multitone => AudioCodec::MultitoneGenerator,
                    AssetKind::PinkLike => AudioCodec::PinkLikeGenerator,
                    AssetKind::Wav => unreachable!(),
                };
                (
                    OriginalMedia {
                        content_sha256: descriptor_sha256.clone(),
                        format: MediaFormat {
                            container: MediaContainer::DeterministicGenerator,
                            codec,
                            sample_rate_hz: self.sample_rate_hz,
                            layout: AssetLayout::Mono,
                            lossy: false,
                        },
                    },
                    PinnedTool {
                        implementation: "fightbox_evidence::signal".into(),
                        revision: "fightbox.asset-descriptor.v1-generator".into(),
                        settings_sha256: descriptor_sha256.clone(),
                    },
                    MotionProvenance {
                        recording_carries_motion: false,
                        evidence: MotionEvidence::AuthoredDry,
                        description: "deterministic generator contains no recorded source motion"
                            .into(),
                    },
                    RightsProvenance {
                        status: RightsStatus::Generated,
                        license: "repository-license:Apache-2.0".into(),
                        evidence:
                            "generated by fightbox_evidence::signal from the hashed descriptor"
                                .into(),
                    },
                    Seekability::DeterministicGenerator,
                )
            }
            AssetKind::Wav => {
                let wav = self.generator.wav.as_ref().expect("validated WAV block");
                let path = resolve_wav_path(&wav.path);
                let bytes = std::fs::read(&path).map_err(|error| {
                    format!("cannot read WAV asset {}: {error}", path.display())
                })?;
                let decoded = decode_source_wav_mono(&bytes, &path)?;
                (
                    OriginalMedia {
                        content_sha256: wav.sha256.clone(),
                        format: MediaFormat {
                            container: MediaContainer::Wav,
                            codec: decoded.codec,
                            sample_rate_hz: decoded.sample_rate_hz,
                            layout: AssetLayout::Mono,
                            lossy: false,
                        },
                    },
                    PinnedTool {
                        implementation: "fightbox-cli::asset::decode_source_wav".into(),
                        revision: "v1-pcm16-or-f32".into(),
                        settings_sha256: descriptor_sha256.clone(),
                    },
                    MotionProvenance {
                        recording_carries_motion: false,
                        evidence: MotionEvidence::LegacyUnspecified,
                        description: "legacy descriptor did not record motion provenance; false is an unverified compatibility default"
                            .into(),
                    },
                    RightsProvenance {
                        status: RightsStatus::LegacyUnverified,
                        license: "unspecified-by-fightbox.asset-descriptor.v1".into(),
                        evidence: format!("legacy descriptor sha256:{descriptor_sha256}"),
                    },
                    Seekability::SampleAccurate,
                )
            }
        };
        let measured = analyzed.analysis();
        let levels = LevelMeasurements {
            rms_dbfs: f64::from(measured.program_rms_dbfs),
            true_peak_dbtp: f64::from(measured.true_peak_dbtp),
        };
        let contract = SourceAssetDescriptor {
            schema_version: SOURCE_ASSET.into(),
            asset_id: self.asset_id.clone(),
            layout: AssetLayout::Mono,
            presentation_provenance: PresentationProvenance::NativeMono,
            compatible_geometries: vec![
                SourceGeometry::Point,
                SourceGeometry::MultiPoint,
                SourceGeometry::LineSegment,
            ],
            original,
            canonical: CanonicalArtifact {
                artifact_id: format!("{CANONICAL_ID_PREFIX}{canonical_pcm_sha256}"),
                canonical_pcm_sha256,
                format: MediaFormat {
                    container: MediaContainer::FightboxPlanarChunksV1,
                    codec: AudioCodec::PcmF32PlanarLe,
                    sample_rate_hz: CANONICAL_RATE_HZ,
                    layout: AssetLayout::Mono,
                    lossy: false,
                },
                frame_count: u64::try_from(signal.samples.len())
                    .map_err(|_| "legacy PCM frame count does not fit u64".to_string())?,
                chunk_frames: CANONICAL_CHUNK_FRAMES,
                chunk_storage: CanonicalChunkStorage::RawOrZstdIfSmaller,
            },
            canonicalization: CanonicalizationProvenance {
                decoder,
                resampler: PinnedTool {
                    implementation: "fightbox-identity-resampler".into(),
                    revision: "48000-to-48000-v1".into(),
                    settings_sha256: sha256_hex(b"identity|input=48000|output=48000"),
                },
            },
            measurements: SourceAssetMeasurements {
                analysis_revision: ASSET_ANALYSIS_METHOD_ID.into(),
                per_channel: vec![ChannelMeasurements {
                    channel: ChannelLabel::Mono,
                    levels,
                }],
                aggregate: levels,
                stereo: None,
            },
            motion,
            rights,
            seekability,
            derivation: None,
        };
        contract.validate()?;
        Ok(contract)
    }
}

impl ResolvedAsset {
    /// Regenerate the deterministic mono PCM and analyze it with the real
    /// ebur128-backed analyzer. The returned signal carries the generator
    /// normalization record; the analysis is the decoded, pre-drive program RMS.
    pub fn regenerate_mono(&self) -> Result<(GeneratedSignal, AnalyzedAsset), String> {
        let descriptor = &self.descriptor;
        // Phase A fixtures bind mono assets; the evidence generator supports
        // stereo duplication, but the source calibration chain operates on the
        // mono program. Reject a stereo descriptor so the one-gain chain stays
        // bound to a single channel aggregation.
        if descriptor.channels != 1 {
            return Err("Phase A asset descriptors must declare mono channels".into());
        }
        let spec = WavSpec {
            sample_rate_hz: descriptor.sample_rate_hz,
            channels: 1,
        };
        let target = descriptor.target_rms_dbfs as f32;
        let signal = match descriptor.kind {
            AssetKind::Sine => {
                let frequency = descriptor.generator.sine.unwrap().frequency_hz as f32;
                sine(spec, frequency, self.frame_count, target)
            }
            AssetKind::Multitone => {
                let frequencies: Vec<f32> = descriptor
                    .generator
                    .multitone
                    .as_ref()
                    .unwrap()
                    .frequencies_hz
                    .iter()
                    .map(|&v| v as f32)
                    .collect();
                multitone(spec, &frequencies, self.frame_count, target)
            }
            AssetKind::PinkLike => {
                let seed = descriptor.generator.pink_like.unwrap().seed;
                pink_like(spec, seed, self.frame_count, target)
            }
            AssetKind::Wav => return self.load_wav_mono(),
        }
        .map_err(map_signal_error)?;
        if !signal.samples.iter().all(|s| s.is_finite()) {
            return Err("regenerated asset PCM is not finite".into());
        }
        let analysis = signal.analyze().map_err(|e| {
            format!(
                "regenerated asset analysis failed: {}",
                asset_analysis_message(&e)
            )
        })?;
        Ok((signal, analysis))
    }

    fn load_wav_mono(&self) -> Result<(GeneratedSignal, AnalyzedAsset), String> {
        let descriptor = &self.descriptor;
        let wav = descriptor.generator.wav.as_ref().unwrap();
        let path = resolve_wav_path(&wav.path);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("cannot read WAV asset {}: {error}", path.display()))?;
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != wav.sha256 {
            return Err(format!(
                "WAV asset {} sha256 mismatch: descriptor {}, file {}",
                path.display(),
                wav.sha256,
                actual_hash
            ));
        }

        let decoded = decode_source_wav_mono(&bytes, &path)?;
        if decoded.sample_rate_hz != 48_000 {
            return Err(format!(
                "WAV asset {} must be 48000 Hz, got {} Hz",
                path.display(),
                decoded.sample_rate_hz
            ));
        }
        if decoded.channels != 1 {
            return Err(format!(
                "WAV asset {} must be mono, got {} channels",
                path.display(),
                decoded.channels
            ));
        }

        let source_frames = decoded.samples.len();
        let start = usize::try_from(wav.start_frame)
            .map_err(|_| "generator.wav.start_frame does not fit this platform".to_string())?;
        if start >= source_frames {
            return Err(format!(
                "generator.wav.start_frame {} is outside WAV asset with {} frames",
                wav.start_frame, source_frames
            ));
        }
        let mut samples = Vec::with_capacity(self.frame_count);
        for output_frame in 0..self.frame_count {
            let source_frame = start + output_frame;
            let sample = if wav.r#loop {
                decoded.samples[source_frame % source_frames]
            } else {
                decoded.samples.get(source_frame).copied().unwrap_or(0.0)
            };
            samples.push(sample);
        }

        let normalization =
            normalize_file_samples(&mut samples, descriptor.target_rms_dbfs as f32)?;
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        };
        // GeneratedSignal predates file-backed assets and its closed SignalKind
        // enum has no Wav member. Consumers use the public PCM/spec/normalization
        // fields, so retain the broadband compatibility tag until that shared
        // evidence type can be evolved in a separately owned change.
        let signal = GeneratedSignal {
            kind: SignalKind::PinkLike,
            spec,
            samples,
            normalization,
        };
        let analysis = signal.analyze().map_err(|error| {
            format!(
                "loaded WAV asset analysis failed: {}",
                asset_analysis_message(&error)
            )
        })?;
        Ok((signal, analysis))
    }
}

fn resolve_wav_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

pub(crate) struct DecodedSourceWav {
    pub(crate) sample_rate_hz: u32,
    pub(crate) channels: u16,
    pub(crate) codec: AudioCodec,
    pub(crate) samples: Vec<f32>,
}

pub(crate) fn decode_source_wav(bytes: &[u8], path: &Path) -> Result<DecodedSourceWav, String> {
    decode_source_wav_inner(bytes, path, None)
}

fn decode_source_wav_mono(bytes: &[u8], path: &Path) -> Result<DecodedSourceWav, String> {
    decode_source_wav_inner(bytes, path, Some(1))
}

fn decode_source_wav_inner(
    bytes: &[u8],
    path: &Path,
    required_channels: Option<u16>,
) -> Result<DecodedSourceWav, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!(
            "WAV asset {} has a malformed RIFF/WAVE header",
            path.display()
        ));
    }
    let mut format = None;
    let mut data = None;
    let mut position = 12usize;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size = u32::from_le_bytes(
            bytes[position + 4..position + 8]
                .try_into()
                .expect("four-byte chunk size"),
        ) as usize;
        position += 8;
        let end = position
            .checked_add(size)
            .ok_or_else(|| format!("WAV asset {} has an oversized chunk", path.display()))?;
        if end > bytes.len() {
            return Err(format!(
                "WAV asset {} has a truncated chunk",
                path.display()
            ));
        }
        if id == b"fmt " {
            if size < 16 {
                return Err(format!(
                    "WAV asset {} has a short fmt chunk",
                    path.display()
                ));
            }
            let body = &bytes[position..end];
            format = Some((
                u16::from_le_bytes([body[0], body[1]]),
                u16::from_le_bytes([body[2], body[3]]),
                u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                u16::from_le_bytes([body[12], body[13]]),
                u16::from_le_bytes([body[14], body[15]]),
            ));
        } else if id == b"data" {
            data = Some(&bytes[position..end]);
        }
        position = end
            .checked_add(size & 1)
            .ok_or_else(|| format!("WAV asset {} has invalid chunk alignment", path.display()))?;
    }
    let (format_tag, channels, sample_rate_hz, block_align, bits_per_sample) =
        format.ok_or_else(|| format!("WAV asset {} is missing its fmt chunk", path.display()))?;
    let data =
        data.ok_or_else(|| format!("WAV asset {} is missing its data chunk", path.display()))?;
    if channels != 1 && channels != 2 {
        return Err(format!(
            "WAV asset {} must be mono or stereo LR, got {} channels",
            path.display(),
            channels
        ));
    }
    if required_channels == Some(1) && channels != 1 {
        return Err(format!(
            "WAV asset {} must be mono, got {} channels",
            path.display(),
            channels
        ));
    }
    if sample_rate_hz == 0 {
        return Err(format!(
            "WAV asset {} has a zero sample rate",
            path.display()
        ));
    }
    let (sample_bytes, codec) = match (format_tag, bits_per_sample) {
        (1, 16) => (2usize, AudioCodec::PcmInteger),
        (3, 32) => (4usize, AudioCodec::PcmFloat),
        (1, bits) => {
            return Err(format!(
                "WAV asset {} must use 16-bit integer PCM or 32-bit IEEE float PCM, got integer PCM with {bits} bits",
                path.display()
            ));
        }
        (3, bits) => {
            return Err(format!(
                "WAV asset {} must use 32-bit IEEE float PCM, got {bits} bits",
                path.display()
            ));
        }
        (tag, _) => {
            return Err(format!(
                "WAV asset {} has unsupported WAV format tag {tag}; expected 1 (integer PCM) or 3 (IEEE float PCM)",
                path.display()
            ));
        }
    };
    if usize::from(block_align) != sample_bytes * usize::from(channels) {
        return Err(format!(
            "WAV asset {} has block_align {}, expected {}",
            path.display(),
            block_align,
            sample_bytes * usize::from(channels)
        ));
    }
    if data.is_empty() || data.len() % usize::from(block_align) != 0 {
        return Err(format!(
            "WAV asset {} has empty or incomplete sample data",
            path.display()
        ));
    }
    let samples = if sample_bytes == 2 {
        data.chunks_exact(2)
            .map(|chunk| f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32768.0)
            .collect()
    } else {
        let mut samples = Vec::with_capacity(data.len() / 4);
        for chunk in data.chunks_exact(4) {
            let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if !sample.is_finite() {
                return Err(format!(
                    "WAV asset {} contains a non-finite sample",
                    path.display()
                ));
            }
            samples.push(sample);
        }
        samples
    };
    Ok(DecodedSourceWav {
        sample_rate_hz,
        channels,
        codec,
        samples,
    })
}

fn hash_planar_f32(samples: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(samples.len() * std::mem::size_of::<f32>());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    sha256_hex(&bytes)
}

fn normalize_file_samples(
    samples: &mut [f32],
    target_rms_dbfs: f32,
) -> Result<GeneratorNormalization, String> {
    let sum_squares = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>();
    let raw_rms = (sum_squares / samples.len() as f64).sqrt() as f32;
    if raw_rms <= 0.0 {
        return Err("WAV asset selection is silent; calibration gain is undefined".into());
    }
    let raw_rms_dbfs = 20.0 * raw_rms.log10();
    let normalization_gain_db = target_rms_dbfs - raw_rms_dbfs;
    let gain = 10.0_f32.powf(normalization_gain_db / 20.0);
    for sample in samples.iter_mut() {
        *sample *= gain;
    }
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if peak > 1.0 {
        return Err(format!(
            "WAV asset calibration gain would push the absolute peak past 1.0 (peak {peak}); no silent clipping"
        ));
    }
    Ok(GeneratorNormalization {
        raw_rms_dbfs,
        target_rms_dbfs,
        normalization_gain_db,
    })
}

fn check_frequency(freq: f64, nyquist: f64) -> Result<(), String> {
    if !freq.is_finite() || freq <= 0.0 {
        return Err(format!("frequency {freq} must be finite and positive"));
    }
    if freq >= nyquist {
        return Err(format!(
            "frequency {freq} must be below Nyquist ({nyquist})"
        ));
    }
    Ok(())
}

fn map_signal_error(error: SignalError) -> String {
    format!("asset regeneration failed: {}", error.as_str())
}

fn asset_analysis_message(error: &fightbox_evidence::AssetAnalysisError) -> &'static str {
    error.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONSET_LOOP: &str = r#"{
  "schema_version": "fightbox.asset-descriptor.v1",
  "asset_id": "onset-loop-example",
  "kind": "wav",
  "generator": {
    "wav": {
      "path": "fixtures/assets/example/onset-loop.wav",
      "sha256": "b284d05165a539f674f9c4d8358fc4bf9dc080b7b6a5510058ed59035a1e75b4",
      "start_frame": 0,
      "loop": true
    }
  },
  "channels": 1,
  "sample_rate_hz": 48000,
  "duration_s": 26.400833333,
  "target_rms_dbfs": -22.036692,
  "expected_reference_rms_dbfs": -20.558544,
  "calibration": {
    "applied_gain_db": -1.478147
  },
  "non_claims": [
    "This descriptor makes no delivered-ear-SPL claim without output calibration."
  ],
  "onsets_s": [
    0.0,
    6.546,
    14.604145833,
    20.115625
  ]
}"#;

    #[test]
    fn composed_loop_onset_tables_parse_to_exact_frames_and_validate_order() {
        let descriptor = AssetDescriptor::parse(ONSET_LOOP).unwrap();
        assert_eq!(descriptor.onsets_s, [0.0, 6.546, 14.604145833, 20.115625]);
        assert_eq!(
            descriptor.onset_frames().unwrap(),
            [0, 314_208, 700_999, 965_550]
        );

        let original = ONSET_LOOP;
        let descending = original.replace("0.0,\n    6.546", "6.546,\n    0.0");
        assert!(
            AssetDescriptor::parse(&descending)
                .unwrap_err()
                .contains("strictly ascending")
        );
        let outside = original.replace("20.115625", "26.400833333");
        assert!(
            AssetDescriptor::parse(&outside)
                .unwrap_err()
                .contains("[0, duration_s)")
        );

        let no_table = AssetDescriptor::parse(TOMS_DINER).unwrap();
        assert!(no_table.onsets_s.is_empty());
        assert!(no_table.onset_frames().unwrap().is_empty());
    }
    use std::sync::atomic::{AtomicU64, Ordering};

    const PINK: &str = include_str!("../../../fixtures/assets/s0-calibrated-pink.json");
    const SINE: &str = include_str!("../../../fixtures/assets/s0-approach-sine-1k.json");
    const MULTITONE: &str = include_str!("../../../fixtures/assets/s3-multitone-spectral.json");
    const AUTHORED_STEREO: &str =
        include_str!("../../../fixtures/assets/source-contract-authored-stereo.json");
    const TOMS_DINER: &str = include_str!("../../../fixtures/assets/toms-diner.json");
    const TEST_WAV: &[u8] = include_bytes!("../testdata/mono-48k-s16.wav");
    const TEST_WAV_SHA256: &str =
        "af32656c8e98bd9f15400108ab770a2e90792c1308136286bbdb88259277cdf7";
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn canonical_stereo_contract_is_strict_and_admits_authored_stereo_image() {
        let descriptor = SourceAssetDescriptor::parse(AUTHORED_STEREO).unwrap();
        assert_eq!(descriptor.layout, AssetLayout::StereoLR);
        assert_eq!(
            descriptor.presentation_provenance,
            PresentationProvenance::AuthoredStereo
        );
        assert_eq!(descriptor.original.format.container, MediaContainer::M4a);
        assert!(descriptor.original.format.lossy);
        assert_eq!(descriptor.canonical.format.sample_rate_hz, 48_000);
        assert!(descriptor.measurements.stereo.is_some());
        assert!(descriptor.motion.recording_carries_motion);
        assert_eq!(descriptor.seekability, Seekability::SampleAccurate);
        assert!(matches!(
            descriptor
                .admit_presentation(SourceGeometry::StereoImage)
                .unwrap(),
            SourcePresentation::AuthoredStereoImage
        ));

        let mut unknown: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        unknown["canonical"]["unversioned_guess"] = serde_json::json!(true);
        assert!(
            SourceAssetDescriptor::parse(&unknown.to_string())
                .unwrap_err()
                .contains("unknown field")
        );
    }

    #[test]
    fn source_contract_rejects_unsupported_axes_with_actionable_errors() {
        let descriptor = SourceAssetDescriptor::parse(AUTHORED_STEREO).unwrap();
        assert!(
            descriptor
                .validate_geometry(SourceGeometry::StereoImage)
                .is_ok()
        );
        let point_error = descriptor
            .admit_presentation(SourceGeometry::Point)
            .unwrap_err();
        let PresentationAdmissionError::StereoPointRequiresExplicitMonoDerivative(offer) =
            point_error
        else {
            panic!("expected explicit PCA-center derivative offer");
        };
        assert_eq!(offer.source_artifact_id, descriptor.canonical.artifact_id);
        assert_eq!(offer.recipe_id, PCA_CENTER_MONO_DERIVATIVE_RECIPE_ID);
        assert_eq!(
            offer.center_weights_lr,
            descriptor
                .measurements
                .stereo
                .unwrap()
                .pca
                .center_weights_lr
        );
        assert_eq!(
            offer.derive_planar_f32(&[1.0, -0.5], &[0.0, 0.5]).unwrap(),
            [0.707_106_77, 0.0]
        );
        assert!(
            descriptor
                .validate_geometry(SourceGeometry::LineSegment)
                .unwrap_err()
                .contains("deferred in V1")
        );
        assert!(
            descriptor
                .validate_motion_use(true)
                .unwrap_err()
                .contains("static-proxy")
        );

        let mut invalid: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        invalid["presentation_provenance"] = serde_json::json!("native_mono");
        let error = SourceAssetDescriptor::parse(&invalid.to_string()).unwrap_err();
        assert!(
            error.contains("native_mono requires layout mono"),
            "got {error}"
        );

        let runtime_error = AssetDescriptor::parse(AUTHORED_STEREO).unwrap_err();
        assert!(
            runtime_error.contains("canonical package/runtime adapter"),
            "got {runtime_error}"
        );
    }

    #[test]
    fn source_contract_rejects_identity_toolchain_and_mono_compatibility_drift() {
        let mut value: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        value["canonical"]["artifact_id"] =
            serde_json::json!(format!("{CANONICAL_ID_PREFIX}{}", "9".repeat(64)));
        assert!(
            SourceAssetDescriptor::parse(&value.to_string())
                .unwrap_err()
                .contains("immutable PCM identity")
        );

        let mut value: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        value["canonicalization"]["decoder"]["revision"] = serde_json::json!("");
        assert!(
            SourceAssetDescriptor::parse(&value.to_string())
                .unwrap_err()
                .contains("pinned identities")
        );

        let mut value: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        value["measurements"]["stereo"]["mono_compatibility"]["short_lag_comb_correlation_delta"] =
            serde_json::json!(0.100_001);
        assert!(
            SourceAssetDescriptor::parse(&value.to_string())
                .unwrap_err()
                .contains("mono-incompatible")
        );
    }

    #[test]
    fn legacy_mono_normalizes_deterministically_into_the_same_contract() {
        let direct = AssetDescriptor::parse(SINE)
            .unwrap()
            .normalize_source_contract()
            .unwrap();
        let compatible = SourceAssetDescriptor::parse(SINE).unwrap();
        assert_eq!(direct, compatible);
        assert_eq!(direct.layout, AssetLayout::Mono);
        assert_eq!(
            direct.presentation_provenance,
            PresentationProvenance::NativeMono
        );
        assert_eq!(
            direct.compatible_geometries,
            [
                SourceGeometry::Point,
                SourceGeometry::MultiPoint,
                SourceGeometry::LineSegment
            ]
        );
        assert_eq!(
            direct.admit_presentation(SourceGeometry::Point).unwrap(),
            SourcePresentation::NativeMono {
                geometry: SourceGeometry::Point
            }
        );

        let reparsed = SourceAssetDescriptor::parse(&format!("\n {SINE} \n")).unwrap();
        assert_eq!(direct.canonical.artifact_id, reparsed.canonical.artifact_id);
        assert_eq!(
            direct.original.content_sha256,
            reparsed.original.content_sha256
        );
        assert_eq!(direct.measurements, reparsed.measurements);

        let reordered = r#"{
          "non_claims": [
            "This descriptor makes no delivered-ear-SPL claim without output calibration.",
            "A single sine is a calibration reference for distance-level ordering, not a perceptual stimulus."
          ],
          "calibration": {"applied_gain_db": null},
          "expected_reference_rms_dbfs": -3.0102999566398125,
          "target_rms_dbfs": -20.0,
          "duration_s": 0.1,
          "sample_rate_hz": 48000,
          "channels": 1,
          "generator": {"sine": {"frequency_hz": 1000.0}, "module": "fightbox_evidence::signal"},
          "kind": "sine",
          "asset_id": "s0-approach-sine-1k",
          "schema_version": "fightbox.asset-descriptor.v1"
        }"#;
        let reordered = SourceAssetDescriptor::parse(reordered).unwrap();
        assert_eq!(
            direct.canonical.artifact_id,
            reordered.canonical.artifact_id
        );
        assert_eq!(
            direct.original.content_sha256,
            reordered.original.content_sha256
        );
    }

    #[test]
    fn locked_axis_table_covers_native_mono_authored_stereo_and_mono_expanded() {
        let native = SourceAssetDescriptor::parse(SINE).unwrap();
        for geometry in [
            SourceGeometry::Point,
            SourceGeometry::MultiPoint,
            SourceGeometry::LineSegment,
        ] {
            native.validate_geometry(geometry).unwrap();
        }
        assert!(
            native
                .validate_geometry(SourceGeometry::StereoImage)
                .is_err()
        );

        let authored = SourceAssetDescriptor::parse(AUTHORED_STEREO).unwrap();
        authored
            .validate_geometry(SourceGeometry::StereoImage)
            .unwrap();
        assert!(
            authored
                .validate_geometry(SourceGeometry::MultiPoint)
                .is_err()
        );

        let mut expanded = native.clone();
        expanded.presentation_provenance = PresentationProvenance::MonoExpanded;
        expanded.compatible_geometries = vec![SourceGeometry::StereoImage];
        expanded.derivation = Some(PresentationDerivation {
            source_artifact_id: expanded.canonical.artifact_id.clone(),
            recipe_sha256: mono_expansion_recipe_sha256(),
        });
        expanded.validate().unwrap();
        expanded
            .validate_geometry(SourceGeometry::StereoImage)
            .unwrap();
        assert!(expanded.validate_geometry(SourceGeometry::Point).is_err());
        assert_eq!(
            expanded
                .admit_presentation(SourceGeometry::StereoImage)
                .unwrap(),
            SourcePresentation::MonoExpandedStereoImage
        );
        assert_eq!(
            SourcePresentation::MonoExpandedStereoImage.program_plane_count(),
            2
        );

        let mut unpinned = expanded.clone();
        unpinned.derivation.as_mut().unwrap().recipe_sha256 = "6".repeat(64);
        assert!(
            unpinned
                .validate()
                .unwrap_err()
                .contains(MONO_EXPANSION_RECIPE_ID)
        );

        for (layout, provenance, expected) in [
            (
                AssetLayout::Mono,
                PresentationProvenance::AuthoredStereo,
                "authored_stereo requires layout stereo_lr",
            ),
            (
                AssetLayout::StereoLR,
                PresentationProvenance::NativeMono,
                "native_mono requires layout mono",
            ),
            (
                AssetLayout::StereoLR,
                PresentationProvenance::MonoExpanded,
                "mono_expanded is a derived mono presentation",
            ),
        ] {
            let mut invalid = authored.clone();
            invalid.layout = layout;
            invalid.presentation_provenance = provenance;
            let error = invalid.validate().unwrap_err();
            assert!(error.contains(expected), "got {error}");
        }
    }

    #[test]
    fn named_toms_diner_regression_remains_mono_point_without_media_decode() {
        let descriptor = AssetDescriptor::parse(TOMS_DINER).unwrap();
        assert_eq!(descriptor.asset_id, "toms-diner");
        assert_eq!(descriptor.kind, AssetKind::Wav);
        assert_eq!(descriptor.channels, 1);
        assert_eq!(descriptor.sample_rate_hz, CANONICAL_RATE_HZ);
        assert_eq!(
            descriptor
                .admit_legacy_presentation(SourceGeometry::Point)
                .unwrap(),
            SourcePresentation::NativeMono {
                geometry: SourceGeometry::Point
            }
        );
        assert!(
            descriptor
                .admit_legacy_presentation(SourceGeometry::StereoImage)
                .unwrap_err()
                .contains("do not implicitly expand")
        );
    }

    #[test]
    fn canonical_contract_accepts_only_the_locked_ingest_format_pairs() {
        for (container, codec, lossy) in [
            ("wav", "pcm_integer", false),
            ("wav", "pcm_float", false),
            ("aiff", "pcm_integer", false),
            ("caf", "pcm_float", false),
            ("flac", "flac", false),
            ("m4a", "aac", true),
            ("mp3", "mp3", true),
        ] {
            let mut value: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
            value["original"]["format"]["container"] = serde_json::json!(container);
            value["original"]["format"]["codec"] = serde_json::json!(codec);
            value["original"]["format"]["lossy"] = serde_json::json!(lossy);
            SourceAssetDescriptor::parse(&value.to_string()).unwrap_or_else(|error| {
                panic!("{container}/{codec}/{lossy} should be admitted: {error}")
            });
        }

        let mut invalid: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        invalid["original"]["format"]["container"] = serde_json::json!("flac");
        invalid["original"]["format"]["codec"] = serde_json::json!("flac");
        invalid["original"]["format"]["lossy"] = serde_json::json!(true);
        assert!(
            SourceAssetDescriptor::parse(&invalid.to_string())
                .unwrap_err()
                .contains("unsupported container/codec/lossy")
        );
    }

    #[test]
    fn authored_stereo_requires_complete_deterministic_pca_analysis() {
        let mut missing: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        missing["measurements"]
            .as_object_mut()
            .unwrap()
            .remove("stereo");
        assert!(
            SourceAssetDescriptor::parse(&missing.to_string())
                .unwrap_err()
                .contains("require PCA, correlation, and mono compatibility")
        );

        let mut non_orthogonal: serde_json::Value = serde_json::from_str(AUTHORED_STEREO).unwrap();
        non_orthogonal["measurements"]["stereo"]["pca"]["width_weights_lr"] =
            serde_json::json!([0.7071067811865476, 0.7071067811865476]);
        assert!(
            SourceAssetDescriptor::parse(&non_orthogonal.to_string())
                .unwrap_err()
                .contains("orthonormal")
        );
    }

    #[test]
    fn parses_all_repo_descriptors() {
        for (text, expected_kind) in [
            (PINK, AssetKind::PinkLike),
            (SINE, AssetKind::Sine),
            (MULTITONE, AssetKind::Multitone),
        ] {
            let descriptor = AssetDescriptor::parse(text).unwrap();
            assert_eq!(descriptor.kind, expected_kind);
            assert_eq!(descriptor.channels, 1);
            assert_eq!(descriptor.sample_rate_hz, 48_000);
        }
    }

    #[test]
    fn rejects_unknown_field() {
        let mut text = SINE.trim_end().to_string();
        text.pop();
        text.push_str(r#","__unknown": true}"#);
        assert!(AssetDescriptor::parse(&text).is_err());
    }

    #[test]
    fn rejects_kind_generator_mismatch() {
        // sine kind but pink_like block present.
        let bad = SINE.replace(r#""kind": "sine""#, r#""kind": "pink_like""#);
        assert!(AssetDescriptor::parse(&bad).is_err());
    }

    #[test]
    fn rejects_above_nyquist_frequency() {
        let bad = SINE.replace("1000.0", "30000.0");
        assert!(AssetDescriptor::parse(&bad).is_err());
    }

    #[test]
    fn regenerates_and_analyzes_repo_pink() {
        let descriptor = AssetDescriptor::parse(PINK).unwrap();
        let resolved = descriptor.resolve().unwrap();
        assert_eq!(resolved.frame_count, 4_800);
        let (signal, analysis) = resolved.regenerate_mono().unwrap();
        assert_eq!(signal.samples.len(), 4_800);
        let rms = analysis.analysis().program_rms_dbfs;
        // The generator normalizes to the declared -20 dBFS target.
        assert!((rms - (-20.0)).abs() < 0.05, "got {rms}");
    }

    fn wav_descriptor(
        path: &Path,
        hash: &str,
        start_frame: u64,
        looping: bool,
        frames: usize,
    ) -> String {
        serde_json::json!({
            "schema_version": "fightbox.asset-descriptor.v1",
            "asset_id": "test-file-backed-wav",
            "kind": "wav",
            "generator": {
                "wav": {
                    "path": path,
                    "sha256": hash,
                    "start_frame": start_frame,
                    "loop": looping
                }
            },
            "channels": 1,
            "sample_rate_hz": 48000,
            "duration_s": frames as f64 / 48000.0,
            "target_rms_dbfs": -40.0,
            "expected_reference_rms_dbfs": null,
            "calibration": {"applied_gain_db": null},
            "non_claims": [
                "This descriptor makes no delivered-ear-SPL claim without output calibration."
            ]
        })
        .to_string()
    }

    fn test_wav_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("mono-48k-s16.wav")
    }

    fn temp_wav(bytes: &[u8]) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fightbox-asset-test-{}-{sequence}.wav",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn parses_wav_descriptor_and_rejects_missing_hash_or_generator_mismatch() {
        let text = wav_descriptor(&test_wav_path(), TEST_WAV_SHA256, 0, false, 128);
        let descriptor = AssetDescriptor::parse(&text).unwrap();
        assert_eq!(descriptor.kind, AssetKind::Wav);
        assert_eq!(descriptor.generator.wav.unwrap().start_frame, 0);

        let mut missing_hash: serde_json::Value = serde_json::from_str(&text).unwrap();
        missing_hash["generator"]["wav"]
            .as_object_mut()
            .unwrap()
            .remove("sha256");
        assert!(
            AssetDescriptor::parse(&missing_hash.to_string())
                .unwrap_err()
                .contains("missing field `sha256`")
        );

        let mismatched = text.replace(r#""kind":"wav""#, r#""kind":"sine""#);
        assert!(
            AssetDescriptor::parse(&mismatched)
                .unwrap_err()
                .contains("requires exactly")
        );
    }

    #[test]
    fn rejects_wav_with_wrong_hash_rate_or_channels() {
        let wrong_hash = wav_descriptor(&test_wav_path(), &"0".repeat(64), 0, false, 128);
        assert!(
            AssetDescriptor::parse(&wrong_hash)
                .unwrap()
                .resolve()
                .unwrap()
                .regenerate_mono()
                .unwrap_err()
                .contains("sha256 mismatch")
        );

        for (offset, replacement, expected) in [
            (
                24usize,
                44_100u32.to_le_bytes().to_vec(),
                "must be 48000 Hz",
            ),
            (22usize, 2u16.to_le_bytes().to_vec(), "must be mono"),
        ] {
            let mut bytes = TEST_WAV.to_vec();
            bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
            let path = temp_wav(&bytes);
            let text = wav_descriptor(&path, &sha256_hex(&bytes), 0, false, 128);
            let error = AssetDescriptor::parse(&text)
                .unwrap()
                .resolve()
                .unwrap()
                .regenerate_mono()
                .unwrap_err();
            assert!(error.contains(expected), "got {error}");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn wav_start_frame_loops_or_pads_after_end() {
        let decoded = decode_source_wav(TEST_WAV, &test_wav_path()).unwrap();
        let start = decoded.samples.len() - 6;
        let frames = 128;

        let looping = AssetDescriptor::parse(&wav_descriptor(
            &test_wav_path(),
            TEST_WAV_SHA256,
            start as u64,
            true,
            frames,
        ))
        .unwrap()
        .resolve()
        .unwrap()
        .regenerate_mono()
        .unwrap()
        .0;
        let loop_gain = 10.0_f32.powf(looping.normalization.normalization_gain_db / 20.0);
        for (output_frame, actual) in looping.samples.iter().copied().enumerate() {
            let expected =
                decoded.samples[(start + output_frame) % decoded.samples.len()] * loop_gain;
            assert!((actual - expected).abs() < 1e-7);
        }

        let padded = AssetDescriptor::parse(&wav_descriptor(
            &test_wav_path(),
            TEST_WAV_SHA256,
            start as u64,
            false,
            frames,
        ))
        .unwrap()
        .resolve()
        .unwrap()
        .regenerate_mono()
        .unwrap()
        .0;
        let pad_gain = 10.0_f32.powf(padded.normalization.normalization_gain_db / 20.0);
        for output_frame in 0..6 {
            let expected = decoded.samples[start + output_frame] * pad_gain;
            assert!((padded.samples[output_frame] - expected).abs() < 1e-7);
        }
        assert!(padded.samples[6..].iter().all(|sample| *sample == 0.0));
    }
}
