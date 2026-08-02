use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) const MIN_SOURCE_OFFSET_DB: f32 = -24.0;
pub(crate) const MAX_SOURCE_OFFSET_DB: f32 = 24.0;
pub(crate) const MIN_MONITOR_GAIN_DB: f32 = -20.0;
pub(crate) const MAX_MONITOR_GAIN_DB: f32 = 40.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct MixDefaults {
    pub(crate) schema_version: u32,
    pub(crate) monitor_gain_db: f32,
    pub(crate) sources: Vec<SourceMixDefault>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceMixDefault {
    pub(crate) id: String,
    pub(crate) enabled: bool,
    pub(crate) muted: bool,
    pub(crate) soloed: bool,
    pub(crate) monitor_offset_db: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedMixDefaults {
    pub(crate) monitor_gain_db: f32,
    pub(crate) sources: BTreeMap<String, SourceMixDefault>,
    pub(crate) ignored_source_ids: Vec<String>,
}

impl MixDefaults {
    pub(crate) const SCHEMA_VERSION: u32 = 1;

    pub(crate) fn read(fixture_path: &Path) -> Result<Option<Self>, String> {
        let path = sidecar_path(fixture_path);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let defaults: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid mix defaults {}: {error}", path.display()))?;
        defaults.validate()?;
        Ok(Some(defaults))
    }

    pub(crate) fn write(&self, fixture_path: &Path) -> Result<PathBuf, String> {
        self.validate()?;
        let path = sidecar_path(fixture_path);
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode mix defaults: {error}"))?;
        std::fs::write(&path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        Ok(path)
    }

    pub(crate) fn resolve(
        &self,
        valid_source_ids: impl IntoIterator<Item = String>,
    ) -> ResolvedMixDefaults {
        let valid_source_ids = valid_source_ids.into_iter().collect::<BTreeSet<_>>();
        let mut sources = BTreeMap::new();
        let mut ignored_source_ids = Vec::new();
        for source in &self.sources {
            if valid_source_ids.contains(&source.id) {
                let mut source = source.clone();
                source.monitor_offset_db = clamp_source_offset_db(source.monitor_offset_db);
                sources.insert(source.id.clone(), source);
            } else {
                ignored_source_ids.push(source.id.clone());
            }
        }
        ResolvedMixDefaults {
            monitor_gain_db: self
                .monitor_gain_db
                .clamp(MIN_MONITOR_GAIN_DB, MAX_MONITOR_GAIN_DB),
            sources,
            ignored_source_ids,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(format!(
                "unsupported mix-defaults schema version {}",
                self.schema_version
            ));
        }
        if !self.monitor_gain_db.is_finite() {
            return Err("mix-default monitor gain must be finite".into());
        }
        if self
            .sources
            .iter()
            .any(|source| source.id.trim().is_empty() || !source.monitor_offset_db.is_finite())
        {
            return Err("mix-default source ids must be non-empty and offsets finite".into());
        }
        Ok(())
    }
}

pub(crate) fn sidecar_path(fixture_path: &Path) -> PathBuf {
    fixture_path.with_extension("user.json")
}

pub(crate) fn clamp_source_offset_db(offset_db: f32) -> f32 {
    offset_db.clamp(MIN_SOURCE_OFFSET_DB, MAX_SOURCE_OFFSET_DB)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn defaults() -> MixDefaults {
        MixDefaults {
            schema_version: MixDefaults::SCHEMA_VERSION,
            monitor_gain_db: -7.5,
            sources: vec![
                SourceMixDefault {
                    id: "bells".into(),
                    enabled: true,
                    muted: false,
                    soloed: true,
                    monitor_offset_db: -6.0,
                },
                SourceMixDefault {
                    id: "stale-id".into(),
                    enabled: false,
                    muted: true,
                    soloed: false,
                    monitor_offset_db: 8.0,
                },
            ],
        }
    }

    #[test]
    fn sidecar_round_trip_parses_applies_and_ignores_mismatched_ids() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!(
            "fightbox-workbench-mix-defaults-{}-{nonce}.json",
            std::process::id()
        ));
        let expected_path = fixture.with_extension("user.json");
        let written = defaults().write(&fixture).unwrap();
        assert_eq!(written, expected_path);

        let parsed = MixDefaults::read(&fixture).unwrap().unwrap();
        assert_eq!(parsed, defaults());
        let resolved = parsed.resolve(["bells".to_owned(), "siren".to_owned()]);
        assert_eq!(resolved.monitor_gain_db, -7.5);
        assert_eq!(resolved.sources["bells"].monitor_offset_db, -6.0);
        assert_eq!(resolved.ignored_source_ids, ["stale-id"]);

        std::fs::remove_file(expected_path).unwrap();
    }

    #[test]
    fn source_offsets_are_clamped_to_the_monitor_trim_range() {
        assert_eq!(clamp_source_offset_db(-30.0), MIN_SOURCE_OFFSET_DB);
        assert_eq!(clamp_source_offset_db(30.0), MAX_SOURCE_OFFSET_DB);
        assert_eq!(clamp_source_offset_db(3.25), 3.25);

        let mut defaults = defaults();
        defaults.sources[0].monitor_offset_db = 99.0;
        assert_eq!(
            defaults.resolve(["bells".to_owned()]).sources["bells"].monitor_offset_db,
            MAX_SOURCE_OFFSET_DB
        );
    }
}
