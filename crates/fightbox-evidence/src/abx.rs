//! Human-completed externalization ABX session records.
//!
//! The schema is deliberately small and deterministic. One record binds an
//! on-device session to the tuple (listener identity, HRTF set, device,
//! head-tracking state), and every trial preserves enough of the randomized
//! presentation to audit its `correct` flag.

use std::fmt;

use crate::json::{JsonObject, json_string_array};
use crate::listening::{EquipmentRecord, HrtfRecord, ListenerIdentity, SignOff};

pub const ABX_SCHEMA_VERSION: &str = "fightbox.abx.v1";
pub const ABX_MIN_TRIALS: usize = 10;

/// Statement attached to every ABX record so generated scaffolding cannot be a pass.
pub const ABX_REQUIRES_HUMAN: &str =
    "Human on-device completion and sign-off are required; this template alone is not a pass.";

/// One forced-choice trial.
///
/// `presented_order` contains exactly one each of `"A"`, `"B"`, and either
/// `"X=A"` or `"X=B"`, in the order presented to the listener. `response` is
/// `"A"` or `"B"`.
#[derive(Clone, Debug, PartialEq)]
pub struct AbxTrialRecord {
    pub trial_index: u32,
    pub presented_order: Vec<String>,
    pub response: String,
    pub correct: bool,
}

/// One complete externalization ABX session.
///
/// The identifying key is (`listener.listener_id`, `hrtf.hrtf_set`, `device`,
/// `head_tracking_enabled`). A different value in any component requires a
/// separate session record.
#[derive(Clone, Debug, PartialEq)]
pub struct AbxSessionRecord {
    pub schema_version: String,
    pub session_id: String,
    pub listener: ListenerIdentity,
    pub hrtf: HrtfRecord,
    pub equipment: EquipmentRecord,
    pub device: String,
    pub head_tracking_enabled: bool,
    pub seed: u64,
    pub trials: Vec<AbxTrialRecord>,
    pub date_iso: String,
    pub sign_off: SignOff,
    pub claims: Vec<String>,
    pub non_claims: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AbxValidationError {
    MissingField(&'static str),
    TooFewTrials { minimum: usize, actual: usize },
    NonContiguousTrialIndex { expected: u32, actual: u32 },
    InvalidPresentedOrder { trial_index: u32 },
    InvalidResponse { trial_index: u32 },
    IncorrectCorrectnessFlag { trial_index: u32 },
}

impl fmt::Display for AbxValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "ABX record is missing {field}"),
            Self::TooFewTrials { minimum, actual } => {
                write!(
                    f,
                    "ABX record requires at least {minimum} trials, found {actual}"
                )
            }
            Self::NonContiguousTrialIndex { expected, actual } => write!(
                f,
                "ABX trial indices must be contiguous and one-based: expected {expected}, found {actual}"
            ),
            Self::InvalidPresentedOrder { trial_index } => write!(
                f,
                "ABX trial {trial_index} must present A, B, and exactly one X=A or X=B token"
            ),
            Self::InvalidResponse { trial_index } => {
                write!(f, "ABX trial {trial_index} response must be A or B")
            }
            Self::IncorrectCorrectnessFlag { trial_index } => write!(
                f,
                "ABX trial {trial_index} correct flag does not match its X assignment and response"
            ),
        }
    }
}

impl std::error::Error for AbxValidationError {}

impl AbxSessionRecord {
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        listener: ListenerIdentity,
        hrtf: HrtfRecord,
        equipment: EquipmentRecord,
        device: impl Into<String>,
        head_tracking_enabled: bool,
        seed: u64,
        sign_off: SignOff,
        date_iso: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: ABX_SCHEMA_VERSION.into(),
            session_id: session_id.into(),
            listener,
            hrtf,
            equipment,
            device: device.into(),
            head_tracking_enabled,
            seed,
            trials: vec![],
            date_iso: date_iso.into(),
            sign_off,
            claims: vec![],
            non_claims: vec![ABX_REQUIRES_HUMAN.into()],
        }
    }

    #[must_use]
    pub fn correct_count(&self) -> usize {
        self.trials.iter().filter(|trial| trial.correct).count()
    }

    #[must_use]
    pub fn trial_count(&self) -> usize {
        self.trials.len()
    }

    /// Exact one-sided binomial tail `P(X >= correct_count | p = 0.5)`.
    ///
    /// ABX sessions are intentionally small, so this directly sums the
    /// binomial probability mass instead of introducing a statistics
    /// dependency or a normal approximation.
    #[must_use]
    pub fn exact_binomial_one_sided_p_value(&self) -> f64 {
        exact_binomial_tail(self.correct_count(), self.trial_count())
    }

    pub fn validate(&self) -> Result<(), AbxValidationError> {
        for (field, value) in [
            ("schema_version", self.schema_version.as_str()),
            ("session_id", self.session_id.as_str()),
            ("listener.listener_id", self.listener.listener_id.as_str()),
            ("hrtf.hrtf_set", self.hrtf.hrtf_set.as_str()),
            ("hrtf.pretest_result", self.hrtf.pretest_result.as_str()),
            ("equipment.headphones", self.equipment.headphones.as_str()),
            ("equipment.output_path", self.equipment.output_path.as_str()),
            ("device", self.device.as_str()),
            ("date_iso", self.date_iso.as_str()),
            (
                "sign_off.listener_signed",
                self.sign_off.listener_signed.as_str(),
            ),
            ("sign_off.date_iso", self.sign_off.date_iso.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(AbxValidationError::MissingField(field));
            }
        }

        if self.trials.len() < ABX_MIN_TRIALS {
            return Err(AbxValidationError::TooFewTrials {
                minimum: ABX_MIN_TRIALS,
                actual: self.trials.len(),
            });
        }

        for (offset, trial) in self.trials.iter().enumerate() {
            let expected_index = u32::try_from(offset + 1).unwrap_or(u32::MAX);
            if trial.trial_index != expected_index {
                return Err(AbxValidationError::NonContiguousTrialIndex {
                    expected: expected_index,
                    actual: trial.trial_index,
                });
            }
            let Some(answer) = x_assignment(&trial.presented_order) else {
                return Err(AbxValidationError::InvalidPresentedOrder {
                    trial_index: trial.trial_index,
                });
            };
            if trial.response != "A" && trial.response != "B" {
                return Err(AbxValidationError::InvalidResponse {
                    trial_index: trial.trial_index,
                });
            }
            if trial.correct != (trial.response == answer) {
                return Err(AbxValidationError::IncorrectCorrectnessFlag {
                    trial_index: trial.trial_index,
                });
            }
        }

        Ok(())
    }

    /// Serialize using the fixed-order, whitespace-free `fightbox.abx.v1`
    /// byte convention.
    pub fn to_json(&self) -> Result<String, AbxValidationError> {
        self.validate()?;

        let mut o = JsonObject::new();
        o.str("schema_version", &self.schema_version);
        o.str("session_id", &self.session_id);

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

        o.str("device", &self.device);
        o.boolean("head_tracking_enabled", self.head_tracking_enabled);
        o.num_u64("seed", self.seed);

        let mut trials = String::from("[");
        for (index, trial) in self.trials.iter().enumerate() {
            if index > 0 {
                trials.push(',');
            }
            let mut trial_json = JsonObject::new();
            trial_json.num_u32("trial_index", trial.trial_index);
            trial_json.raw_value(
                "presented_order",
                &json_string_array(trial.presented_order.iter().map(String::as_str)),
            );
            trial_json.str("response", &trial.response);
            trial_json.boolean("correct", trial.correct);
            trials.push_str(&trial_json.finish());
        }
        trials.push(']');
        o.raw_value("trials", &trials);

        o.num_usize("correct_count", self.correct_count());
        o.num_usize("trial_count", self.trial_count());
        o.raw_value(
            "exact_binomial_one_sided_p_value",
            &format!("{}", self.exact_binomial_one_sided_p_value()),
        );
        o.str("date_iso", &self.date_iso);

        let mut sign_off = JsonObject::new();
        sign_off.str("listener_signed", &self.sign_off.listener_signed);
        sign_off.str("date_iso", &self.sign_off.date_iso);
        o.raw_value("sign_off", &sign_off.finish());

        o.boolean("requires_human_completion", true);
        o.raw_value(
            "claims",
            &json_string_array(self.claims.iter().map(String::as_str)),
        );
        o.raw_value(
            "non_claims",
            &json_string_array(self.non_claims.iter().map(String::as_str)),
        );
        Ok(o.finish())
    }
}

fn x_assignment(presented_order: &[String]) -> Option<&str> {
    if presented_order.len() != 3 {
        return None;
    }
    let a_count = presented_order.iter().filter(|item| *item == "A").count();
    let b_count = presented_order.iter().filter(|item| *item == "B").count();
    let x_answers = presented_order
        .iter()
        .filter_map(|item| item.strip_prefix("X="))
        .collect::<Vec<_>>();
    if a_count == 1
        && b_count == 1
        && x_answers.len() == 1
        && (x_answers[0] == "A" || x_answers[0] == "B")
    {
        Some(x_answers[0])
    } else {
        None
    }
}

fn exact_binomial_tail(correct_count: usize, trial_count: usize) -> f64 {
    if correct_count == 0 {
        return 1.0;
    }

    let mut combination = 1.0;
    let mut numerator = 0.0;
    for successes in 0..=trial_count {
        if successes >= correct_count {
            numerator += combination;
        }
        if successes < trial_count {
            combination *= (trial_count - successes) as f64 / (successes + 1) as f64;
        }
    }
    numerator * 0.5_f64.powf(trial_count as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_sample(correct_count: usize) -> AbxSessionRecord {
        let mut record = AbxSessionRecord::new(
            "externalization-0001",
            ListenerIdentity {
                listener_id: "listener-a".into(),
                notes: "on-device".into(),
            },
            HrtfRecord {
                hrtf_set: "steam-audio-default".into(),
                pretest_result: "pass".into(),
            },
            EquipmentRecord {
                headphones: "AirPods Pro".into(),
                output_path: "iPhone Bluetooth".into(),
                monitor_gain_db: None,
            },
            "iPhone 16 Pro",
            true,
            0xABCD,
            SignOff {
                listener_signed: "listener-a".into(),
                date_iso: "2026-07-30".into(),
            },
            "2026-07-30",
        );
        record.trials = (0..10)
            .map(|index| {
                let answer = if index % 2 == 0 { "A" } else { "B" };
                let response = if index < correct_count {
                    answer
                } else if answer == "A" {
                    "B"
                } else {
                    "A"
                };
                AbxTrialRecord {
                    trial_index: index as u32 + 1,
                    presented_order: vec!["B".into(), format!("X={answer}"), "A".into()],
                    response: response.into(),
                    correct: response == answer,
                }
            })
            .collect();
        record
    }

    #[test]
    fn exact_binomial_tail_matches_known_cases() {
        let nine_of_ten = complete_sample(9);
        assert_eq!(nine_of_ten.correct_count(), 9);
        assert_eq!(nine_of_ten.trial_count(), 10);
        assert!((nine_of_ten.exact_binomial_one_sided_p_value() - 0.010_742_187_5).abs() < 1e-15);

        let ten_of_ten = complete_sample(10);
        assert!((ten_of_ten.exact_binomial_one_sided_p_value() - 0.000_976_562_5).abs() < 1e-15);

        let five_of_ten = complete_sample(5);
        assert!((five_of_ten.exact_binomial_one_sided_p_value() - 0.623_046_875).abs() < 1e-15);
    }

    #[test]
    fn serialization_is_deterministic_and_fixed_order() {
        let record = complete_sample(9);
        let first = record.to_json().unwrap();
        assert_eq!(first, record.to_json().unwrap());
        assert!(first.starts_with(
            r#"{"schema_version":"fightbox.abx.v1","session_id":"externalization-0001""#
        ));
        assert!(first.contains(r#""presented_order":["B","X=A","A"]"#));
        assert!(first.contains(
            r#""correct_count":9,"trial_count":10,"exact_binomial_one_sided_p_value":0.0107421875"#
        ));
        assert!(first.contains("\"requires_human_completion\":true"));
        assert!(first.contains(ABX_REQUIRES_HUMAN));
    }

    #[test]
    fn incomplete_session_is_rejected() {
        let mut record = complete_sample(9);
        record.trials.pop();
        assert_eq!(
            record.to_json(),
            Err(AbxValidationError::TooFewTrials {
                minimum: 10,
                actual: 9,
            })
        );
    }
}
