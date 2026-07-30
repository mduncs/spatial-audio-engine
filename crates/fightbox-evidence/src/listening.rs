//! Provisional S3 listening-record vocabulary.
//!
//! This is the in-code mirror of `docs/listening/s3-listening-record.schema.json`.
//! Phase A listening is provisional: human ears only, because Gate 0 (the
//! ears-library self-validation) does not exist yet. Every record therefore
//! carries [`LISTENING_REQUIRES_HUMAN`] as a non-claim and a `requires_human_completion`
//! flag, and the template alone is never a pass.

use crate::json::{JsonObject, json_string_array};

pub const LISTENING_SCHEMA_VERSION: &str = "fightbox.listening.v1";

/// Statement attached to every record so the template alone is never a pass.
pub const LISTENING_REQUIRES_HUMAN: &str =
    "Human completion is required; this template alone is not a pass.";

#[derive(Clone, Debug, PartialEq)]
pub struct ListenerIdentity {
    pub listener_id: String,
    pub notes: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HrtfRecord {
    pub hrtf_set: String,
    pub pretest_result: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EquipmentRecord {
    pub headphones: String,
    pub output_path: String,
    pub monitor_gain_db: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListeningObservation {
    pub stimulus: String,
    pub observation: String,
}

/// One listener-facing question in the qualification pack.
#[derive(Clone, Debug, PartialEq)]
pub struct PerceptPrompt {
    pub percept: String,
    pub wav_file: String,
    pub pass_criteria: String,
}

/// Scene metadata used to create a blank, scene-specific record.
#[derive(Clone, Debug, PartialEq)]
pub struct ListeningPromptSet {
    pub scene_id: String,
    pub fixture_id: String,
    pub gate: String,
    pub prompts: Vec<PerceptPrompt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListeningResult {
    Undecided,
    Pass,
    Fail,
}
impl ListeningResult {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Undecided => "undecided",
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignOff {
    pub listener_signed: String,
    pub date_iso: String,
}

/// A provisional S3 listening record. See `docs/listening/`.
#[derive(Clone, Debug, PartialEq)]
pub struct ListeningRecord {
    pub schema_version: String,
    pub record_id: String,
    pub fixture_id: String,
    pub gate: String,
    pub fixture_sha256: Option<String>,
    pub bundle_manifest_sha256: Option<String>,
    pub listener: ListenerIdentity,
    pub hrtf: HrtfRecord,
    pub equipment: EquipmentRecord,
    pub comparison_order: Vec<String>,
    pub percept_prompts: Vec<PerceptPrompt>,
    pub observations: Vec<ListeningObservation>,
    pub result: ListeningResult,
    pub date_iso: String,
    pub sign_off: SignOff,
    pub claims: Vec<String>,
    pub non_claims: Vec<String>,
}

impl ListeningRecord {
    /// Build a record, always stamping the human-required non-claim so the
    /// template can never silently pose as a pass.
    #[must_use]
    pub fn new(
        record_id: impl Into<String>,
        fixture_id: impl Into<String>,
        listener: ListenerIdentity,
        hrtf: HrtfRecord,
        equipment: EquipmentRecord,
        sign_off: SignOff,
        date_iso: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: LISTENING_SCHEMA_VERSION.into(),
            record_id: record_id.into(),
            fixture_id: fixture_id.into(),
            gate: "S3".into(),
            fixture_sha256: None,
            bundle_manifest_sha256: None,
            listener,
            hrtf,
            equipment,
            comparison_order: vec!["pathing_on".into(), "pathing_off".into()],
            percept_prompts: vec![],
            observations: vec![],
            result: ListeningResult::Undecided,
            date_iso: date_iso.into(),
            sign_off,
            claims: vec![],
            non_claims: vec![LISTENING_REQUIRES_HUMAN.into()],
        }
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        let mut o = JsonObject::new();
        o.str("schema_version", &self.schema_version);
        o.str("record_id", &self.record_id);
        o.str("fixture_id", &self.fixture_id);
        o.str("gate", &self.gate);
        o.opt_str("fixture_sha256", self.fixture_sha256.as_deref());
        o.opt_str(
            "bundle_manifest_sha256",
            self.bundle_manifest_sha256.as_deref(),
        );

        let mut listener = JsonObject::new();
        listener.str("listener_id", &self.listener.listener_id);
        listener.str("notes", &self.listener.notes);
        o.raw_value("listener", &listener.finish());

        let mut hrtf = JsonObject::new();
        hrtf.str("hrtf_set", &self.hrtf.hrtf_set);
        hrtf.str("pretest_result", &self.hrtf.pretest_result);
        o.raw_value("hrtf", &hrtf.finish());

        let mut equipment = JsonObject::new();
        equipment.str("headphones", &self.equipment.headphones);
        equipment.str("output_path", &self.equipment.output_path);
        equipment.opt_f32("monitor_gain_db", self.equipment.monitor_gain_db);
        o.raw_value("equipment", &equipment.finish());

        o.raw_value(
            "comparison_order",
            &json_string_array(self.comparison_order.iter().map(|s| s.as_str())),
        );

        if !self.percept_prompts.is_empty() {
            let mut prompts = String::from("[");
            for (i, prompt) in self.percept_prompts.iter().enumerate() {
                if i > 0 {
                    prompts.push(',');
                }
                let mut prompt_json = JsonObject::new();
                prompt_json.str("percept", &prompt.percept);
                prompt_json.str("wav_file", &prompt.wav_file);
                prompt_json.str("pass_criteria", &prompt.pass_criteria);
                prompts.push_str(&prompt_json.finish());
            }
            prompts.push(']');
            o.raw_value("percept_prompts", &prompts);
        }

        let mut observations = String::from("[");
        for (i, obs) in self.observations.iter().enumerate() {
            if i > 0 {
                observations.push(',');
            }
            let mut obsj = JsonObject::new();
            obsj.str("stimulus", &obs.stimulus);
            obsj.str("observation", &obs.observation);
            observations.push_str(&obsj.finish());
        }
        observations.push(']');
        o.raw_value("observations", &observations);

        o.str("result", self.result.as_str());
        o.str("date_iso", &self.date_iso);

        let mut sign_off = JsonObject::new();
        sign_off.str("listener_signed", &self.sign_off.listener_signed);
        sign_off.str("date_iso", &self.sign_off.date_iso);
        o.raw_value("sign_off", &sign_off.finish());

        // The template is provisional until a human completes it.
        o.boolean("requires_human_completion", true);

        let claims = json_string_array(self.claims.iter().map(|s| s.as_str()));
        let non_claims = json_string_array(self.non_claims.iter().map(|s| s.as_str()));
        o.raw_value("claims", &claims);
        o.raw_value("non_claims", &non_claims);
        o.finish()
    }
}

/// The complete Phase D/§σ listening inventory. Wording is deliberately in
/// listener language: each item says what to attend to and what an audible
/// pass sounds like without asking the listener to interpret a metric.
#[must_use]
pub fn qualification_prompt_sets() -> Vec<ListeningPromptSet> {
    vec![
        prompt_set(
            "s1-azimuth-rotation",
            "s1-free-field-rotation",
            "S1",
            &[(
                "Azimuth and head rotation",
                "LISTEN-s1-azimuth-rotation.wav",
                "The source stays at a stable distance, moves smoothly around the head in the stated direction, and never jumps, reverses, or collapses into the center.",
            )],
        ),
        prompt_set(
            "s2-occlusion-filter",
            "s2-direct-occlusion",
            "S2",
            &[(
                "Occlusion filter",
                "LISTEN-s2-occlusion-filter.wav",
                "When the obstruction enters, the source becomes clearly more muffled and quieter without muting, clicking, or changing pitch; it clears smoothly when line of sight returns.",
            )],
        ),
        prompt_set(
            "s3-corner-handoff",
            "s3-masonry-building-corner",
            "S3",
            &[
                (
                    "Around-corner pathing on",
                    "LISTEN-s3-pathing-on-sum.wav",
                    "The hidden source remains audible as an arrival from the corner rather than from through the wall.",
                ),
                (
                    "Around-corner pathing off control",
                    "LISTEN-s3-pathing-off-sum.wav",
                    "The control lacks the convincing around-corner arrival heard in the pathing-on file.",
                ),
                (
                    "Line-of-sight handoff",
                    "LISTEN-s3-trajectory-sum.wav",
                    "The source crosses between hidden and visible continuously: direction and tone hand off at the corner with no jump, click, dropout, or double image.",
                ),
            ],
        ),
        prompt_set(
            "s4-scene-family",
            "s4-room-canyon-doorway",
            "S4",
            &[
                (
                    "Room decay and material ordering",
                    "LISTEN-s4-room-masonry-ir.wav",
                    "The masonry room has a clear, natural tail; compared with the high-absorption file, its decay is audibly longer and stronger rather than merely louder at onset.",
                ),
                (
                    "High-absorption room control",
                    "LISTEN-s4-room-high-absorption-ir.wav",
                    "This room dies away sooner and with less late energy than the masonry room, without a click or truncated tail.",
                ),
                (
                    "Street-canyon spaciousness",
                    "LISTEN-s4-canyon-ir.wav",
                    "The canyon opens into a dense, spacious field around the direct event and does not read as one isolated slapback echo.",
                ),
                (
                    "Single-slapback canyon control",
                    "LISTEN-s4-canyon-single-slapback-control-ir.wav",
                    "Compared with the full canyon, this control reads as one obvious return rather than a dense field; the contrast makes the canyon spaciousness unambiguous.",
                ),
                (
                    "Doorway continuity",
                    "LISTEN-s4-doorway-walk-summed.wav",
                    "Level, tone, and apparent direction change continuously through the doorway; there is no state-change click, zipper, dropout, or sudden timbre swap.",
                ),
            ],
        ),
        prompt_set(
            "s5-city-walk",
            "chicago-city-walk",
            "S5",
            &[
                (
                    "City-walk spatial continuity",
                    "LISTEN-s5-city-walk.wav",
                    "The storefront remains anchored while the listener approaches and turns the corner; distance, occlusion, reflections, and direction evolve continuously without pumping, zipper noise, or a teleport.",
                ),
                (
                    "Tom's Diner moving coloration",
                    "LISTEN-toms-diner-walk.wav",
                    "The center-panned vocal stays tonally natural through approach, orbit, and recession: no moving comb-filter whistle, pumping level, zipper, or click is audible.",
                ),
            ],
        ),
        prompt_set(
            "s6a-four-sources",
            "s6a-four-sources-one-moving",
            "S6A",
            &[
                (
                    "Per-source isolation",
                    "LISTEN-s6a-stem-1-occluded.wav",
                    "Only the named northwest source is heard; there is no tone, image, or tail leaking from the other three sources.",
                ),
                (
                    "Moving-source Doppler continuity",
                    "LISTEN-s6a-stem-4-moving.wav",
                    "The moving tone bends pitch and direction continuously through approach and recession, with no step, hiccup, doubled onset, or reset.",
                ),
            ],
        ),
        prompt_set(
            "siren-scene",
            "megablock-siren",
            "Phase D",
            &[(
                "Moving siren continuity",
                "LISTEN-siren-scene.wav",
                "The siren follows a continuous route with stable identity; direction, distance, occlusion, reflections, and Doppler change smoothly without a jump or duplicate image.",
            )],
        ),
        prompt_set(
            "bell-scene",
            "megablock-bell",
            "Phase D",
            &[(
                "Distant bell localization and decay",
                "LISTEN-bell-scene.wav",
                "The bell is localized at a stable distant point and each strike keeps a natural, uninterrupted decay through the city reflections without metallic zipper or pumping.",
            )],
        ),
        prompt_set(
            "firework-scene",
            "s-firework-elevated-megablock",
            "Phase D",
            &[(
                "Elevated firework impulse and city return",
                "LISTEN-firework-scene.wav",
                "A sharp elevated crack arrives first, followed by a lower boom and clearly audible city returns that spread and decay naturally; the onset is not softened, doubled, or detached from the tail.",
            )],
        ),
    ]
}

/// Build one blank record per scene in [`qualification_prompt_sets`].
#[must_use]
pub fn blank_qualification_records() -> Vec<ListeningRecord> {
    qualification_prompt_sets()
        .into_iter()
        .map(|set| {
            let comparison_order = set
                .prompts
                .iter()
                .map(|prompt| prompt.wav_file.clone())
                .collect();
            let observations = set
                .prompts
                .iter()
                .map(|prompt| ListeningObservation {
                    stimulus: prompt.percept.clone(),
                    observation: String::new(),
                })
                .collect();
            ListeningRecord {
                schema_version: LISTENING_SCHEMA_VERSION.into(),
                record_id: format!("{}-listening-record", set.scene_id),
                fixture_id: set.fixture_id,
                gate: set.gate,
                fixture_sha256: None,
                bundle_manifest_sha256: None,
                listener: ListenerIdentity {
                    listener_id: String::new(),
                    notes: String::new(),
                },
                hrtf: HrtfRecord {
                    hrtf_set: "steam-audio-default".into(),
                    pretest_result: "not_run".into(),
                },
                equipment: EquipmentRecord {
                    headphones: String::new(),
                    output_path: String::new(),
                    monitor_gain_db: None,
                },
                comparison_order,
                percept_prompts: set.prompts,
                observations,
                result: ListeningResult::Undecided,
                date_iso: String::new(),
                sign_off: SignOff {
                    listener_signed: String::new(),
                    date_iso: String::new(),
                },
                claims: vec![],
                non_claims: vec![
                    LISTENING_REQUIRES_HUMAN.into(),
                    "No delivered-ear-SPL claim without output-device/headphone calibration."
                        .into(),
                ],
            }
        })
        .collect()
}

fn prompt_set(
    scene_id: &str,
    fixture_id: &str,
    gate: &str,
    prompts: &[(&str, &str, &str)],
) -> ListeningPromptSet {
    ListeningPromptSet {
        scene_id: scene_id.into(),
        fixture_id: fixture_id.into(),
        gate: gate.into(),
        prompts: prompts
            .iter()
            .map(|(percept, wav_file, pass_criteria)| PerceptPrompt {
                percept: (*percept).into(),
                wav_file: (*wav_file).into(),
                pass_criteria: (*pass_criteria).into(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ListeningRecord {
        ListeningRecord::new(
            "s3-provisional-0001",
            "s3-masonry-building-corner",
            ListenerIdentity {
                listener_id: "listener-a".into(),
                notes: "provisional".into(),
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
        )
    }

    #[test]
    fn record_is_deterministic_and_marks_human_required() {
        let record = sample();
        let json = record.to_json();
        assert_eq!(json, record.to_json());
        assert!(json.contains("\"requires_human_completion\":true"));
        assert!(json.contains("Human completion is required"));
        assert!(json.contains("\"result\":\"undecided\""));
        assert!(json.contains("\"comparison_order\":[\"pathing_on\",\"pathing_off\"]"));
    }

    #[test]
    fn non_claims_survive_customization() {
        let mut record = sample();
        record.claims.push("audible on/off difference".into());
        record.non_claims.push("no delivered-ear-SPL claim".into());
        let json = record.to_json();
        assert!(json.contains("no delivered-ear-SPL claim"));
        // The human-required non-claim is retained even after customization.
        assert!(json.contains("Human completion is required"));
    }

    #[test]
    fn qualification_pack_covers_every_required_scene_and_prompt_field() {
        let sets = qualification_prompt_sets();
        let scene_ids = sets
            .iter()
            .map(|set| set.scene_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            scene_ids,
            [
                "s1-azimuth-rotation",
                "s2-occlusion-filter",
                "s3-corner-handoff",
                "s4-scene-family",
                "s5-city-walk",
                "s6a-four-sources",
                "siren-scene",
                "bell-scene",
                "firework-scene",
            ]
        );
        assert!(sets.iter().all(|set| !set.prompts.is_empty()));
        assert!(sets.iter().flat_map(|set| &set.prompts).all(|prompt| {
            !prompt.percept.is_empty()
                && prompt.wav_file.ends_with(".wav")
                && !prompt.pass_criteria.is_empty()
        }));
    }

    #[test]
    fn blank_qualification_records_are_unanswered_and_human_required() {
        let records = blank_qualification_records();
        assert_eq!(records.len(), 9);
        assert!(records.iter().all(|record| {
            record.result == ListeningResult::Undecided
                && record.sign_off.listener_signed.is_empty()
                && record
                    .observations
                    .iter()
                    .all(|observation| observation.observation.is_empty())
                && record
                    .non_claims
                    .iter()
                    .any(|claim| claim == LISTENING_REQUIRES_HUMAN)
        }));
        let json = records.last().unwrap().to_json();
        assert!(json.contains("\"percept\":\"Elevated firework impulse and city return\""));
        assert!(json.contains("\"wav_file\":\"LISTEN-firework-scene.wav\""));
        assert!(json.contains("\"pass_criteria\":"));
    }
}
