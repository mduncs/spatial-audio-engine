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
}
