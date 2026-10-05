use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fixture::Fixture;

#[derive(Debug)]
pub(crate) struct ScenePositions {
    path: PathBuf,
    original: Vec<u8>,
    sources: BTreeMap<String, SourcePosition>,
}

#[derive(Debug)]
struct SourcePosition {
    span: Range<usize>,
    original: [f32; 3],
    moved: Option<[f32; 3]>,
}

impl ScenePositions {
    pub(crate) fn read(path: &Path) -> Result<Self, String> {
        let path = std::fs::canonicalize(path)
            .map_err(|error| format!("cannot resolve fixture {}: {error}", path.display()))?;
        let original = std::fs::read(&path)
            .map_err(|error| format!("cannot read fixture {}: {error}", path.display()))?;
        let fixture = Fixture::parse(&original, &path.display().to_string())?;
        let sources = source_positions(&original, &fixture)?;
        Ok(Self {
            path,
            original,
            sources,
        })
    }

    pub(crate) fn set_position(&mut self, id: &str, position: [f32; 3]) -> Result<(), String> {
        if position.iter().any(|value| !value.is_finite()) {
            return Err(format!("source {id} position must be finite"));
        }
        let source = self
            .sources
            .get_mut(id)
            .ok_or_else(|| format!("source {id} has no editable static position_m"))?;
        source.moved = (position != source.original).then_some(position);
        Ok(())
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.sources.values().any(|source| source.moved.is_some())
    }

    pub(crate) fn save(&mut self) -> Result<(), String> {
        if !self.is_dirty() {
            return Ok(());
        }
        let mut edits = self
            .sources
            .values()
            .filter_map(|source| source.moved.map(|position| (source.span.clone(), position)))
            .collect::<Vec<_>>();
        edits.sort_by_key(|(span, _)| span.start);
        let mut patched = Vec::with_capacity(self.original.len());
        let mut previous = 0;
        for (span, position) in edits {
            patched.extend_from_slice(&self.original[previous..span.start]);
            // Centimetre resolution in the file's own `[x, y, z]` style, without f32 noise.
            let [east, north, up] = position.map(|value| (f64::from(value) * 100.0).round() / 100.0);
            patched.extend_from_slice(format!("[{east}, {north}, {up}]").as_bytes());
            previous = span.end;
        }
        patched.extend_from_slice(&self.original[previous..]);
        let fixture = Fixture::parse(&patched, &self.path.display().to_string())?;
        let sources = source_positions(&patched, &fixture)?;
        self.check_unchanged()?;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("cannot prepare fixture save: {error}"))?
            .as_nanos();
        let mut name = std::ffi::OsString::from(".");
        name.push(
            self.path
                .file_name()
                .ok_or("fixture path has no filename")?,
        );
        name.push(format!(
            ".fightbox-position-{}-{nonce}.tmp",
            std::process::id()
        ));
        let temporary = self.path.with_file_name(name);
        let permissions = std::fs::metadata(&self.path)
            .map_err(|error| format!("cannot inspect fixture: {error}"))?
            .permissions();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("cannot stage fixture save: {error}"))?;
        let result = (|| {
            file.set_permissions(permissions)
                .map_err(|error| format!("cannot preserve fixture permissions: {error}"))?;
            file.write_all(&patched)
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("cannot write fixture save: {error}"))?;
            self.check_unchanged()?;
            std::fs::rename(&temporary, &self.path)
                .map_err(|error| format!("cannot replace fixture {}: {error}", self.path.display()))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        self.original = patched;
        self.sources = sources;
        Ok(())
    }

    fn check_unchanged(&self) -> Result<(), String> {
        let current = std::fs::read(&self.path)
            .map_err(|error| format!("cannot read fixture {}: {error}", self.path.display()))?;
        if current != self.original {
            return Err("fixture changed on disk; reload it before saving source positions".into());
        }
        Ok(())
    }
}

fn source_positions(
    bytes: &[u8],
    fixture: &Fixture,
) -> Result<BTreeMap<String, SourcePosition>, String> {
    let mut scanner = JsonSpans { bytes, cursor: 0 };
    let mut spans = BTreeMap::new();
    scanner.expect(b'{')?;
    while !scanner.consume(b'}') {
        let key = scanner.string()?;
        scanner.expect(b':')?;
        if key == "sources" {
            scanner.expect(b'[')?;
            while !scanner.consume(b']') {
                scanner.expect(b'{')?;
                let mut id = None;
                let mut position = None;
                while !scanner.consume(b'}') {
                    let key = scanner.string()?;
                    scanner.expect(b':')?;
                    if key == "id" {
                        id = Some(scanner.string()?);
                    } else {
                        scanner.whitespace();
                        let start = scanner.cursor;
                        let is_array = scanner.bytes.get(start) == Some(&b'[');
                        scanner.value()?;
                        if key == "position_m" && is_array {
                            position = Some(start..scanner.cursor);
                        }
                    }
                    if !scanner.consume(b',') {
                        scanner.expect(b'}')?;
                        break;
                    }
                }
                if let (Some(id), Some(position)) = (id, position)
                    && spans.insert(id.clone(), position).is_some()
                {
                    return Err(format!("source {id} has an ambiguous duplicate id"));
                }
                if !scanner.consume(b',') {
                    scanner.expect(b']')?;
                    break;
                }
            }
        } else {
            scanner.value()?;
        }
        if !scanner.consume(b',') {
            scanner.expect(b'}')?;
            break;
        }
    }
    let mut sources = BTreeMap::new();
    for source in &fixture.sources {
        if source.trajectory.is_some() {
            continue;
        }
        if let Some(position) = source.position_m {
            let span = spans
                .remove(&source.id)
                .ok_or_else(|| format!("source {} has no unique position_m array", source.id))?;
            sources.insert(
                source.id.clone(),
                SourcePosition {
                    span,
                    original: position.map(|value| value as f32),
                    moved: None,
                },
            );
        }
    }
    Ok(sources)
}

// Only locate spans; Fixture::parse remains the JSON and scene validator.
struct JsonSpans<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl JsonSpans<'_> {
    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.cursor += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        self.whitespace();
        if self.bytes.get(self.cursor) == Some(&byte) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err("cannot locate fixture position arrays".into())
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.whitespace();
        let start = self.cursor;
        self.expect(b'"')?;
        while let Some(&byte) = self.bytes.get(self.cursor) {
            self.cursor += 1;
            if byte == b'\\' {
                self.cursor += 1;
            } else if byte == b'"' {
                return serde_json::from_slice(&self.bytes[start..self.cursor])
                    .map_err(|error| format!("cannot read fixture key: {error}"));
            }
        }
        Err("cannot locate fixture string end".into())
    }

    fn value(&mut self) -> Result<(), String> {
        self.whitespace();
        match self.bytes.get(self.cursor) {
            Some(b'"') => {
                self.string()?;
            }
            Some(b'{') => {
                self.cursor += 1;
                while !self.consume(b'}') {
                    self.string()?;
                    self.expect(b':')?;
                    self.value()?;
                    if !self.consume(b',') {
                        self.expect(b'}')?;
                        break;
                    }
                }
            }
            Some(b'[') => {
                self.cursor += 1;
                while !self.consume(b']') {
                    self.value()?;
                    if !self.consume(b',') {
                        self.expect(b']')?;
                        break;
                    }
                }
            }
            Some(_) => {
                let start = self.cursor;
                while self.bytes.get(self.cursor).is_some_and(|byte| {
                    !byte.is_ascii_whitespace() && ![b',', b']', b'}'].contains(byte)
                }) {
                    self.cursor += 1;
                }
                if start == self.cursor {
                    return Err("cannot locate fixture value".into());
                }
            }
            None => return Err("cannot locate fixture value end".into()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENE: &str = r#"{
  "notes": "escaped \"position_m\": [8, 9, 10], } ] \\",
  "position_m": [90,91,92],
  "sources": [
    { "position_m" : [ 1e1, 20.000, 1.5 ],
      "extra": {"id":"nested", "position_m":[7,8,9]},
      "id":"speaker\"one", "asset_id":"sound", "default_enabled":false,
      "reference_level":{"mode":"SplAtOneMeter","db_spl":100} },
    { "id":"untouched", "position_m":[30,40,1.5], "asset_id":"sound",
      "default_enabled":false, "reference_level":{"mode":"SplAtOneMeter","db_spl":100} }
  ],
  "listener":{"position_m":[50,60,1.5],"forward_enu":[0,1,0]},
  "simulation":{"direct":{},"reflections":{},"pathing":{},"probe_volume":{"spacing_m":8}}
}
"#;

    fn temporary_fixture(bytes: &[u8]) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fightbox-scene-positions-{}-{nonce}.json",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn scene_position_patch_preserves_every_other_byte_and_parses() {
        let path = temporary_fixture(SCENE.as_bytes());
        let mut positions = ScenePositions::read(&path).unwrap();
        assert!(!positions.is_dirty());
        positions
            .set_position("speaker\"one", [12.25, 22.5, 1.5])
            .unwrap();
        positions
            .set_position("untouched", [31.0, 41.0, 1.5])
            .unwrap();
        positions
            .set_position("untouched", [30.0, 40.0, 1.5])
            .unwrap();
        assert!(positions.is_dirty());
        positions.save().unwrap();
        let saved = std::fs::read(&path).unwrap();
        let expected = SCENE.replacen("[ 1e1, 20.000, 1.5 ]", "[12.25, 22.5, 1.5]", 1);
        assert_eq!(saved, expected.as_bytes());
        let parsed = Fixture::parse(&saved, "saved-scene").unwrap();
        assert_eq!(parsed.sources[0].position_m, Some([12.25, 22.5, 1.5]));
        assert!(parsed.sources.iter().all(|source| !source.default_enabled));
        assert!(!positions.is_dirty());
        positions
            .set_position("speaker\"one", [13.0, 23.0, 1.5])
            .unwrap();
        positions.save().unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            expected
                .replacen("[12.25, 22.5, 1.5]", "[13, 23, 1.5]", 1)
                .as_bytes()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn scene_position_save_rejects_external_changes_and_invalid_ballistics() {
        let path = temporary_fixture(SCENE.as_bytes());
        let mut positions = ScenePositions::read(&path).unwrap();
        positions
            .set_position("speaker\"one", [12.0, 22.0, 1.5])
            .unwrap();
        let external = format!("{SCENE}\n");
        std::fs::write(&path, &external).unwrap();
        assert!(positions.save().unwrap_err().contains("changed on disk"));
        assert_eq!(std::fs::read(&path).unwrap(), external.as_bytes());
        assert!(positions.is_dirty());
        std::fs::remove_file(path).unwrap();

        let original =
            include_bytes!("../../../fixtures/city/astra-artillery/street-path-candidate.json");
        let path = temporary_fixture(original);
        let mut positions = ScenePositions::read(&path).unwrap();
        positions
            .set_position("artillery-corner-shot", [202.5, 102.5, 1.5])
            .unwrap();
        assert!(
            positions
                .save()
                .unwrap_err()
                .contains("mach_segments total")
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(positions.is_dirty());
        std::fs::remove_file(path).unwrap();
    }
}
