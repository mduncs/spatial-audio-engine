//! Human listening-pack record initialization and validation.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use fightbox_evidence::{ListeningRecord, blank_qualification_records, qualification_prompt_sets};
use serde_json::Value;

use crate::error::{CliError, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidationSummary {
    pub records: usize,
    pub passed: usize,
    pub failed: usize,
    pub incomplete: usize,
}

impl ValidationSummary {
    #[must_use]
    pub fn is_pass(self) -> bool {
        self.records > 0 && self.failed == 0 && self.incomplete == 0
    }
}

pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("init") => {
            let output = parse_output_arg(&args[1..])?;
            let written = init(&output)?;
            println!(
                "listening init: wrote {} blank record forms (JSON + Markdown) to {}",
                written,
                output.display()
            );
            Ok(())
        }
        Some("validate") if args.len() == 2 => {
            let directory = PathBuf::from(&args[1]);
            let summary = validate(&directory)?;
            let status = if summary.is_pass() { "PASS" } else { "FAIL" };
            println!(
                "listening validate: {status} records={} passed={} failed={} incomplete={}",
                summary.records, summary.passed, summary.failed, summary.incomplete
            );
            if summary.is_pass() {
                Ok(())
            } else {
                Err(CliError::new(format!(
                    "listening qualification did not pass: {} failed, {} incomplete",
                    summary.failed, summary.incomplete
                )))
            }
        }
        Some("validate") => Err(CliError::new(
            "usage: fightbox listening validate <directory>",
        )),
        Some(other) => Err(CliError::new(format!(
            "unknown listening subcommand {other:?}; expected init or validate"
        ))),
        None => Err(CliError::new(
            "listening requires a subcommand: init or validate",
        )),
    }
}

pub fn init(output: &Path) -> Result<usize> {
    fs::create_dir_all(output).map_err(|error| {
        CliError::with(
            format!("create listening output {}", output.display()),
            error,
        )
    })?;
    let records = blank_qualification_records();
    for record in &records {
        let scene_id = record
            .record_id
            .strip_suffix("-listening-record")
            .unwrap_or(&record.record_id);
        let json_path = output.join(format!("listening-{scene_id}.json"));
        let markdown_path = output.join(format!("listening-{scene_id}.md"));
        refuse_overwrite(&json_path)?;
        refuse_overwrite(&markdown_path)?;
        fs::write(&json_path, format!("{}\n", record.to_json()))
            .map_err(|error| CliError::with(format!("write {}", json_path.display()), error))?;
        fs::write(&markdown_path, render_markdown(record))
            .map_err(|error| CliError::with(format!("write {}", markdown_path.display()), error))?;
    }
    Ok(records.len())
}

pub fn validate(directory: &Path) -> Result<ValidationSummary> {
    let expected = qualification_prompt_sets()
        .into_iter()
        .map(|set| format!("{}-listening-record", set.scene_id))
        .collect::<BTreeSet<_>>();
    let mut found = BTreeSet::new();
    let mut summary = ValidationSummary {
        records: 0,
        passed: 0,
        failed: 0,
        incomplete: 0,
    };
    let entries = fs::read_dir(directory).map_err(|error| {
        CliError::with(
            format!("read listening directory {}", directory.display()),
            error,
        )
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| CliError::with("read listening directory entry", error))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("listening-")
            || path.extension().and_then(|ext| ext.to_str()) != Some("json")
        {
            continue;
        }
        let bytes = fs::read(&path)
            .map_err(|error| CliError::with(format!("read {}", path.display()), error))?;
        let record: Value = serde_json::from_slice(&bytes)
            .map_err(|error| CliError::with(format!("parse {}", path.display()), error))?;
        let record_id = string_field(&record, "record_id")
            .ok_or_else(|| CliError::new(format!("{}: missing record_id", path.display())))?;
        if !expected.contains(record_id) {
            return Err(CliError::new(format!(
                "{}: unexpected qualification record_id {record_id:?}",
                path.display()
            )));
        }
        if !found.insert(record_id.to_owned()) {
            return Err(CliError::new(format!(
                "duplicate qualification record_id {record_id:?}"
            )));
        }
        summary.records += 1;

        let observations_complete = record["observations"]
            .as_array()
            .is_some_and(|observations| {
                !observations.is_empty()
                    && observations.iter().all(|observation| {
                        observation["observation"].as_str().is_some_and(is_answer)
                    })
            });
        let prompts_match = match (
            record["percept_prompts"].as_array(),
            record["observations"].as_array(),
        ) {
            (Some(prompts), Some(observations)) => {
                !prompts.is_empty() && prompts.len() == observations.len()
            }
            _ => false,
        };
        let signed = record["sign_off"]["listener_signed"]
            .as_str()
            .is_some_and(is_answer)
            && record["sign_off"]["date_iso"]
                .as_str()
                .is_some_and(is_iso_date);
        let result = string_field(&record, "result").unwrap_or("undecided");

        if !observations_complete || !prompts_match || !signed || result == "undecided" {
            summary.incomplete += 1;
        } else if result == "pass" {
            summary.passed += 1;
        } else {
            summary.failed += 1;
        }
    }

    summary.incomplete += expected.difference(&found).count();
    summary.records += expected.difference(&found).count();
    Ok(summary)
}

fn parse_output_arg(args: &[String]) -> Result<PathBuf> {
    if args.len() == 2 && args[0] == "--output" {
        Ok(PathBuf::from(&args[1]))
    } else {
        Err(CliError::new(
            "usage: fightbox listening init --output <directory>",
        ))
    }
}

fn refuse_overwrite(path: &Path) -> Result<()> {
    if path.exists() {
        Err(CliError::new(format!(
            "refusing to overwrite existing listening form {}",
            path.display()
        )))
    } else {
        Ok(())
    }
}

fn render_markdown(record: &ListeningRecord) -> String {
    let mut markdown = format!(
        "# Listening record: {}\n\nFixture: `{}`  \nGate: `{}`  \nResult: `undecided` (change in the JSON form)\n\n",
        record.record_id, record.fixture_id, record.gate
    );
    for (index, prompt) in record.percept_prompts.iter().enumerate() {
        markdown.push_str(&format!(
            "## {}. {}\n\nWAV: `{}`\n\nPass when: {}\n\nObservation:\n\n\n",
            index + 1,
            prompt.percept,
            prompt.wav_file,
            prompt.pass_criteria
        ));
    }
    markdown.push_str(
        "## Sign-off\n\nListener:  \nDate (YYYY-MM-DD):  \n\nThe JSON form is the record of authority; this Markdown file is the readable worksheet.\n",
    );
    markdown
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field)?.as_str()
}

fn is_answer(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && !trimmed.to_ascii_lowercase().starts_with("replace") && trimmed != "TODO"
}

fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "fightbox-listening-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn init_writes_json_and_readable_markdown_for_every_scene() {
        let directory = test_directory("init");
        assert_eq!(init(&directory).unwrap(), 9);
        let json = fs::read_to_string(directory.join("listening-firework-scene.json")).unwrap();
        let markdown = fs::read_to_string(directory.join("listening-firework-scene.md")).unwrap();
        assert!(json.contains("\"wav_file\":\"LISTEN-firework-scene.wav\""));
        assert!(markdown.contains("Elevated firework impulse and city return"));
        assert!(markdown.contains("Pass when:"));
        assert!(init(&directory).is_err(), "init must preserve filled forms");
    }

    #[test]
    fn validate_rejects_blanks_then_accepts_completed_signed_forms() {
        let directory = test_directory("validate");
        init(&directory).unwrap();
        let blank = validate(&directory).unwrap();
        assert_eq!(
            blank,
            ValidationSummary {
                records: 9,
                passed: 0,
                failed: 0,
                incomplete: 9,
            }
        );

        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            record["result"] = Value::String("pass".into());
            record["sign_off"]["listener_signed"] = Value::String("listener-a".into());
            record["sign_off"]["date_iso"] = Value::String("2026-07-29".into());
            for observation in record["observations"].as_array_mut().unwrap() {
                observation["observation"] = Value::String(
                    "I heard the stated percept without the named corruption.".into(),
                );
            }
            fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        }
        let complete = validate(&directory).unwrap();
        assert_eq!(complete.records, 9);
        assert_eq!(complete.passed, 9);
        assert!(complete.is_pass());
    }
}
