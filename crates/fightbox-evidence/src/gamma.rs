//! Canonical Wave 17 capability-card evidence.
//!
//! A gamma card binds the exact source/world/bake/table identities, frozen
//! atmosphere, voice assignment, isolated stems, resource observation, and one
//! explicit listening judgment. It is deliberately SDK-neutral and does not run
//! a gate by itself.

use crate::json::{JsonObject, json_string_array};

pub const GAMMA_CARD_SCHEMA_VERSION: &str = "fightbox.gamma-card.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GammaCardId {
    TransportPulse,
    ExplosionArtillery,
    Firework,
    SupersonicShot,
    Thunder,
    FastMover,
    Contention,
    OwnerHomeAperture,
    TomsDiner,
    SpectralComposition,
    CellBoundary,
}

impl GammaCardId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TransportPulse => "gamma0_transport_pulse",
            Self::ExplosionArtillery => "gamma1_explosion_artillery",
            Self::Firework => "gamma2_firework",
            Self::SupersonicShot => "gamma3_supersonic_shot",
            Self::Thunder => "gamma4_thunder",
            Self::FastMover => "gamma5_fast_mover",
            Self::Contention => "gamma6_contention",
            Self::OwnerHomeAperture => "gamma7_owner_home_aperture",
            Self::TomsDiner => "gamma8_toms_diner",
            Self::SpectralComposition => "gamma9_spectral_composition",
            Self::CellBoundary => "gamma10_cell_boundary",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GammaCardStatus {
    Planned,
    Captured,
    Passed,
    Failed,
}

impl GammaCardStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Captured => "captured",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// A hash is never silently absent. A card either binds it or states why the
/// artifact does not apply to this fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactBinding {
    Bound { sha256: String },
    NotApplicable { reason: String },
}

impl ArtifactBinding {
    #[must_use]
    pub fn bound(sha256: impl Into<String>) -> Self {
        Self::Bound {
            sha256: sha256.into(),
        }
    }

    #[must_use]
    pub fn not_applicable(reason: impl Into<String>) -> Self {
        Self::NotApplicable {
            reason: reason.into(),
        }
    }

    fn validate(&self, field: &'static str) -> Result<(), GammaValidationError> {
        match self {
            Self::Bound { sha256 } if valid_sha256(sha256) => Ok(()),
            Self::Bound { .. } => Err(GammaValidationError::new(
                field,
                "bound artifact must be 64 lowercase hexadecimal SHA-256 characters",
            )),
            Self::NotApplicable { reason } if !reason.trim().is_empty() => Ok(()),
            Self::NotApplicable { .. } => Err(GammaValidationError::new(
                field,
                "not-applicable artifact requires a reason",
            )),
        }
    }

    fn to_json(&self) -> String {
        let mut object = JsonObject::new();
        match self {
            Self::Bound { sha256 } => {
                object.str("status", "bound");
                object.str("sha256", sha256);
            }
            Self::NotApplicable { reason } => {
                object.str("status", "not_applicable");
                object.str("reason", reason);
            }
        }
        object.finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GammaArtifactSet {
    pub source: ArtifactBinding,
    pub world_package: ArtifactBinding,
    pub bake: ArtifactBinding,
    pub table: ArtifactBinding,
}

impl GammaArtifactSet {
    fn validate(&self) -> Result<(), GammaValidationError> {
        self.source.validate("artifacts.source")?;
        self.world_package.validate("artifacts.world_package")?;
        self.bake.validate("artifacts.bake")?;
        self.table.validate("artifacts.table")
    }

    fn to_json(&self) -> String {
        let mut object = JsonObject::new();
        object.raw_value("source", &self.source.to_json());
        object.raw_value("world_package", &self.world_package.to_json());
        object.raw_value("bake", &self.bake.to_json());
        object.raw_value("table", &self.table.to_json());
        object.finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GammaAtmosphere {
    Frozen {
        observation_id: String,
        temperature_c: f32,
        relative_humidity_percent: f32,
        pressure_pa: f32,
        coefficient_sha256: String,
    },
    NotApplicable {
        reason: String,
    },
}

impl GammaAtmosphere {
    fn validate(&self) -> Result<(), GammaValidationError> {
        match self {
            Self::Frozen {
                observation_id,
                temperature_c,
                relative_humidity_percent,
                pressure_pa,
                coefficient_sha256,
            } => {
                if observation_id.trim().is_empty() {
                    return Err(GammaValidationError::new(
                        "atmosphere.observation_id",
                        "frozen atmosphere requires an observation identity",
                    ));
                }
                if !temperature_c.is_finite()
                    || !relative_humidity_percent.is_finite()
                    || !(0.0..=100.0).contains(relative_humidity_percent)
                    || !pressure_pa.is_finite()
                    || *pressure_pa <= 0.0
                {
                    return Err(GammaValidationError::new(
                        "atmosphere",
                        "frozen weather values must be finite, humidity in 0..=100, and pressure positive",
                    ));
                }
                if !valid_sha256(coefficient_sha256) {
                    return Err(GammaValidationError::new(
                        "atmosphere.coefficient_sha256",
                        "coefficient identity must be a lowercase SHA-256",
                    ));
                }
                Ok(())
            }
            Self::NotApplicable { reason } if !reason.trim().is_empty() => Ok(()),
            Self::NotApplicable { .. } => Err(GammaValidationError::new(
                "atmosphere.reason",
                "not-applicable atmosphere requires a reason",
            )),
        }
    }

    fn to_json(&self) -> String {
        let mut object = JsonObject::new();
        match self {
            Self::Frozen {
                observation_id,
                temperature_c,
                relative_humidity_percent,
                pressure_pa,
                coefficient_sha256,
            } => {
                object.str("status", "frozen");
                object.str("observation_id", observation_id);
                object.num_f32("temperature_c", *temperature_c);
                object.num_f32("relative_humidity_percent", *relative_humidity_percent);
                object.num_f32("pressure_pa", *pressure_pa);
                object.str("coefficient_sha256", coefficient_sha256);
            }
            Self::NotApplicable { reason } => {
                object.str("status", "not_applicable");
                object.str("reason", reason);
            }
        }
        object.finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceAssignment {
    pub source_id: String,
    pub logical_voice: u8,
    pub detail_state: String,
    pub event_role: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GammaStem {
    pub label: String,
    pub content_sha256: String,
    pub channels: u16,
    pub frame_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GammaResourceObservation {
    pub platform: String,
    pub callback_p99_ms: f32,
    pub callback_p999_ms: f32,
    pub peak_rss_mib: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListeningOutcome {
    Pending,
    Pass,
    Fail,
    NotRequired,
}

impl ListeningOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::NotRequired => "not_required",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListeningJudgment {
    pub prompt: String,
    pub listener_id: String,
    pub outcome: ListeningOutcome,
    pub notes: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GammaCard {
    pub run_id: String,
    pub card_id: GammaCardId,
    pub status: GammaCardStatus,
    pub engine_revision: String,
    pub artifacts: GammaArtifactSet,
    pub atmosphere: GammaAtmosphere,
    pub quality_states: Vec<String>,
    pub voice_assignments: Vec<VoiceAssignment>,
    pub isolated_stems: Vec<GammaStem>,
    pub resources: Option<GammaResourceObservation>,
    pub listening: ListeningJudgment,
    pub failure_reason: Option<String>,
}

impl GammaCard {
    pub fn validate(&self) -> Result<(), GammaValidationError> {
        if self.run_id.trim().is_empty() {
            return Err(GammaValidationError::new("run_id", "must not be empty"));
        }
        if self.engine_revision.trim().is_empty() {
            return Err(GammaValidationError::new(
                "engine_revision",
                "must not be empty",
            ));
        }
        self.artifacts.validate()?;
        self.atmosphere.validate()?;
        validate_nonempty_unique(&self.quality_states, "quality_states")?;
        validate_voice_assignments(&self.voice_assignments)?;
        validate_stems(&self.isolated_stems)?;
        validate_listening(&self.listening)?;
        if let Some(resources) = &self.resources {
            validate_resources(resources)?;
        }
        if self
            .failure_reason
            .as_ref()
            .is_some_and(|reason| reason.trim().is_empty())
        {
            return Err(GammaValidationError::new(
                "failure_reason",
                "must be absent or non-empty",
            ));
        }
        match self.status {
            GammaCardStatus::Planned => {}
            GammaCardStatus::Captured => {
                require_capture_payload(self)?;
            }
            GammaCardStatus::Passed => {
                require_capture_payload(self)?;
                if !matches!(
                    self.listening.outcome,
                    ListeningOutcome::Pass | ListeningOutcome::NotRequired
                ) {
                    return Err(GammaValidationError::new(
                        "listening.outcome",
                        "a passed card requires pass or explicit not_required",
                    ));
                }
                if self.failure_reason.is_some() {
                    return Err(GammaValidationError::new(
                        "failure_reason",
                        "a passed card cannot carry a failure reason",
                    ));
                }
            }
            GammaCardStatus::Failed => {
                if self.failure_reason.is_none() && self.listening.outcome != ListeningOutcome::Fail
                {
                    return Err(GammaValidationError::new(
                        "failure_reason",
                        "a failed card needs a mechanical failure reason or failed listening judgment",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Deterministic JSON in fixed field order.
    pub fn to_json(&self) -> Result<String, GammaValidationError> {
        self.validate()?;
        let mut object = JsonObject::new();
        object.str("schema_version", GAMMA_CARD_SCHEMA_VERSION);
        object.str("run_id", &self.run_id);
        object.str("card_id", self.card_id.as_str());
        object.str("status", self.status.as_str());
        object.str("engine_revision", &self.engine_revision);
        object.raw_value("artifacts", &self.artifacts.to_json());
        object.raw_value("atmosphere", &self.atmosphere.to_json());
        object.raw_value(
            "quality_states",
            &json_string_array(self.quality_states.iter().map(String::as_str)),
        );
        object.raw_value("voice_assignments", &voices_json(&self.voice_assignments));
        object.raw_value("isolated_stems", &stems_json(&self.isolated_stems));
        match &self.resources {
            Some(resources) => object.raw_value("resources", &resources_json(resources)),
            None => object.raw_value("resources", "null"),
        }
        object.raw_value("listening", &listening_json(&self.listening));
        object.opt_str("failure_reason", self.failure_reason.as_deref());
        Ok(object.finish())
    }
}

fn require_capture_payload(card: &GammaCard) -> Result<(), GammaValidationError> {
    if card.quality_states.is_empty()
        || card.voice_assignments.is_empty()
        || card.isolated_stems.is_empty()
        || card.resources.is_none()
    {
        return Err(GammaValidationError::new(
            "capture_payload",
            "captured, passed, and promoted cards require quality state, voice assignment, isolated stem, and CPU/RSS observation",
        ));
    }
    Ok(())
}

fn validate_nonempty_unique(
    values: &[String],
    field: &'static str,
) -> Result<(), GammaValidationError> {
    for (index, value) in values.iter().enumerate() {
        if value.trim().is_empty() {
            return Err(GammaValidationError::new(
                field,
                "entries must not be empty",
            ));
        }
        if values[..index].contains(value) {
            return Err(GammaValidationError::new(field, "entries must be unique"));
        }
    }
    Ok(())
}

fn validate_voice_assignments(values: &[VoiceAssignment]) -> Result<(), GammaValidationError> {
    for (index, voice) in values.iter().enumerate() {
        if voice.source_id.trim().is_empty()
            || voice.detail_state.trim().is_empty()
            || voice.event_role.trim().is_empty()
        {
            return Err(GammaValidationError::new(
                "voice_assignments",
                "source, detail state, and event role must be explicit",
            ));
        }
        if values[..index]
            .iter()
            .any(|other| other.logical_voice == voice.logical_voice)
        {
            return Err(GammaValidationError::new(
                "voice_assignments.logical_voice",
                "logical voice indices must be unique",
            ));
        }
    }
    Ok(())
}

fn validate_stems(values: &[GammaStem]) -> Result<(), GammaValidationError> {
    for (index, stem) in values.iter().enumerate() {
        if stem.label.trim().is_empty()
            || !valid_sha256(&stem.content_sha256)
            || !(1..=16).contains(&stem.channels)
            || stem.frame_count == 0
        {
            return Err(GammaValidationError::new(
                "isolated_stems",
                "stem label/hash/channel/frame fields must be valid",
            ));
        }
        if values[..index]
            .iter()
            .any(|other| other.label == stem.label)
        {
            return Err(GammaValidationError::new(
                "isolated_stems.label",
                "stem labels must be unique",
            ));
        }
    }
    Ok(())
}

fn validate_resources(resources: &GammaResourceObservation) -> Result<(), GammaValidationError> {
    if resources.platform.trim().is_empty()
        || !resources.callback_p99_ms.is_finite()
        || resources.callback_p99_ms < 0.0
        || !resources.callback_p999_ms.is_finite()
        || resources.callback_p999_ms < resources.callback_p99_ms
        || !resources.peak_rss_mib.is_finite()
        || resources.peak_rss_mib < 0.0
    {
        return Err(GammaValidationError::new(
            "resources",
            "platform must be named and p99/p99.9/RSS must be finite nonnegative values with p99.9 >= p99",
        ));
    }
    Ok(())
}

fn validate_listening(judgment: &ListeningJudgment) -> Result<(), GammaValidationError> {
    if judgment.prompt.trim().is_empty() || judgment.notes.trim().is_empty() {
        return Err(GammaValidationError::new(
            "listening",
            "the explicit prompt and notes must not be empty",
        ));
    }
    match judgment.outcome {
        ListeningOutcome::Pending | ListeningOutcome::NotRequired
            if !judgment.listener_id.is_empty() =>
        {
            Err(GammaValidationError::new(
                "listening.listener_id",
                "pending or not-required listening has no listener identity",
            ))
        }
        ListeningOutcome::Pass | ListeningOutcome::Fail
            if judgment.listener_id.trim().is_empty() =>
        {
            Err(GammaValidationError::new(
                "listening.listener_id",
                "a human judgment requires a listener identity",
            ))
        }
        _ => Ok(()),
    }
}

fn voices_json(values: &[VoiceAssignment]) -> String {
    let mut output = String::from("[");
    for (index, voice) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        let mut object = JsonObject::new();
        object.str("source_id", &voice.source_id);
        object.num_u32("logical_voice", u32::from(voice.logical_voice));
        object.str("detail_state", &voice.detail_state);
        object.str("event_role", &voice.event_role);
        output.push_str(&object.finish());
    }
    output.push(']');
    output
}

fn stems_json(values: &[GammaStem]) -> String {
    let mut output = String::from("[");
    for (index, stem) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        let mut object = JsonObject::new();
        object.str("label", &stem.label);
        object.str("content_sha256", &stem.content_sha256);
        object.num_u32("channels", u32::from(stem.channels));
        object.num_u64("frame_count", stem.frame_count);
        output.push_str(&object.finish());
    }
    output.push(']');
    output
}

fn resources_json(resources: &GammaResourceObservation) -> String {
    let mut object = JsonObject::new();
    object.str("platform", &resources.platform);
    object.num_f32("callback_p99_ms", resources.callback_p99_ms);
    object.num_f32("callback_p999_ms", resources.callback_p999_ms);
    object.num_f32("peak_rss_mib", resources.peak_rss_mib);
    object.finish()
}

fn listening_json(judgment: &ListeningJudgment) -> String {
    let mut object = JsonObject::new();
    object.str("prompt", &judgment.prompt);
    object.str("listener_id", &judgment.listener_id);
    object.str("outcome", judgment.outcome.as_str());
    object.str("notes", &judgment.notes);
    object.finish()
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GammaValidationError {
    pub field: &'static str,
    pub message: &'static str,
}

impl GammaValidationError {
    const fn new(field: &'static str, message: &'static str) -> Self {
        Self { field, message }
    }
}

impl std::fmt::Display for GammaValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for GammaValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn card() -> GammaCard {
        GammaCard {
            run_id: "gamma5-fast-mover-167mps-a".into(),
            card_id: GammaCardId::FastMover,
            status: GammaCardStatus::Passed,
            engine_revision: "feed1234".into(),
            artifacts: GammaArtifactSet {
                source: ArtifactBinding::bound(HASH),
                world_package: ArtifactBinding::not_applicable("free-field fixture"),
                bake: ArtifactBinding::not_applicable("free-field fixture"),
                table: ArtifactBinding::not_applicable("no authored table"),
            },
            atmosphere: GammaAtmosphere::NotApplicable {
                reason: "short-range motion fixture".into(),
            },
            quality_states: vec!["desktop_full".into()],
            voice_assignments: vec![VoiceAssignment {
                source_id: "moving-point".into(),
                logical_voice: 0,
                detail_state: "full".into(),
                event_role: "continuous".into(),
            }],
            isolated_stems: vec![GammaStem {
                label: "final_stereo".into(),
                content_sha256: HASH.into(),
                channels: 2,
                frame_count: 144_000,
            }],
            resources: Some(GammaResourceObservation {
                platform: "mac-arm64".into(),
                callback_p99_ms: 0.42,
                callback_p999_ms: 0.73,
                peak_rss_mib: 211.5,
            }),
            listening: ListeningJudgment {
                prompt: "Is the pass continuous and free of zippering?".into(),
                listener_id: "md".into(),
                outcome: ListeningOutcome::Pass,
                notes: "continuous on headphones".into(),
            },
            failure_reason: None,
        }
    }

    #[test]
    fn complete_card_is_valid_and_byte_stable() {
        let card = card();
        let json = card.to_json().unwrap();
        assert_eq!(json, card.to_json().unwrap());
        assert!(json.contains(r#""schema_version":"fightbox.gamma-card.v1""#));
        assert!(json.contains(r#""card_id":"gamma5_fast_mover""#));
        assert!(json.contains(r#""callback_p999_ms":0.73"#));
        assert!(json.contains(r#""outcome":"pass""#));
    }

    #[test]
    fn pass_requires_capture_payload_and_completed_judgment() {
        let mut missing = card();
        missing.resources = None;
        assert_eq!(missing.validate().unwrap_err().field, "capture_payload");

        let mut pending = card();
        pending.listening = ListeningJudgment {
            prompt: "Listen".into(),
            listener_id: String::new(),
            outcome: ListeningOutcome::Pending,
            notes: "awaiting listener".into(),
        };
        assert_eq!(pending.validate().unwrap_err().field, "listening.outcome");
    }

    #[test]
    fn artifact_hashes_and_voice_slots_are_strict() {
        let mut bad_hash = card();
        bad_hash.artifacts.source = ArtifactBinding::bound("abc");
        assert_eq!(bad_hash.validate().unwrap_err().field, "artifacts.source");

        let mut duplicate = card();
        duplicate.voice_assignments.push(VoiceAssignment {
            source_id: "other".into(),
            logical_voice: 0,
            detail_state: "direct_only".into(),
            event_role: "standard_impulse".into(),
        });
        assert_eq!(
            duplicate.validate().unwrap_err().field,
            "voice_assignments.logical_voice"
        );
    }

    #[test]
    fn every_canonical_card_id_is_stable() {
        assert_eq!(
            GammaCardId::TransportPulse.as_str(),
            "gamma0_transport_pulse"
        );
        assert_eq!(
            GammaCardId::ExplosionArtillery.as_str(),
            "gamma1_explosion_artillery"
        );
        assert_eq!(GammaCardId::Firework.as_str(), "gamma2_firework");
        assert_eq!(
            GammaCardId::SupersonicShot.as_str(),
            "gamma3_supersonic_shot"
        );
        assert_eq!(GammaCardId::Thunder.as_str(), "gamma4_thunder");
        assert_eq!(GammaCardId::FastMover.as_str(), "gamma5_fast_mover");
        assert_eq!(GammaCardId::Contention.as_str(), "gamma6_contention");
        assert_eq!(
            GammaCardId::OwnerHomeAperture.as_str(),
            "gamma7_owner_home_aperture"
        );
        assert_eq!(GammaCardId::TomsDiner.as_str(), "gamma8_toms_diner");
        assert_eq!(
            GammaCardId::SpectralComposition.as_str(),
            "gamma9_spectral_composition"
        );
        assert_eq!(GammaCardId::CellBoundary.as_str(), "gamma10_cell_boundary");
    }
}
