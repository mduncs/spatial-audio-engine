use std::path::Path;

use fightbox_api::EnuVector3;
use fightbox_steam_audio::{
    AcousticMaterial, BakedProbeBatch, DirectOcclusionMode, PROBE_BATCH_METADATA_SCHEMA,
    ProbeBatchMetadata, ReflectionEffectConfig, S3SimulationConfig, STEAM_AUDIO_UPSTREAM_COMMIT,
    STEAM_AUDIO_VERSION, SceneMesh,
};
use fightbox_world::LoadedPackage;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
pub struct Fixture {
    #[serde(default)]
    pub fixture_id: Option<String>,
    pub sources: Vec<FixtureSource>,
    pub listener: FixtureListener,
    pub simulation: FixtureSimulation,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FixtureSource {
    pub id: String,
    pub asset_id: String,
    pub reference_level: FixtureReferenceLevel,
    #[serde(default = "default_enabled")]
    pub default_enabled: bool,
    pub position_m: Option<[f64; 3]>,
    pub trajectory: Option<Trajectory>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixtureReferenceLevel {
    pub db_spl: f64,
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
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixtureDirect {
    pub occlusion_samples: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixtureReflections {
    pub rays: Option<u32>,
    pub bounces: Option<u32>,
    pub duration_s: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FixturePathing {
    pub order: Option<u32>,
    pub validation: Option<bool>,
    pub alternate_paths: Option<bool>,
}

impl Fixture {
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read fixture {}: {error}", path.display()))?;
        let fixture: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid fixture {}: {error}", path.display()))?;
        if fixture.sources.is_empty()
            || fixture.sources.len() > fightbox_runtime::MAX_ACTIVE_SOURCES
        {
            return Err("fixture must contain 1..=MAX_ACTIVE_SOURCES sources".into());
        }
        fixture.initial_listener_position()?;
        for source in &fixture.sources {
            source.initial_position()?;
            if let Some(trajectory) = &source.trajectory {
                trajectory.validate(&format!("source {}", source.id))?;
            }
        }
        if let Some(trajectory) = &fixture.listener.trajectory {
            trajectory.validate("listener")?;
        }
        Ok(fixture)
    }

    pub fn initial_listener_position(&self) -> Result<EnuVector3, String> {
        initial_position(self.listener.position_m, self.listener.trajectory.as_ref())
            .ok_or_else(|| "listener requires a position or non-empty trajectory".into())
    }

    pub fn simulation_config(&self) -> S3SimulationConfig {
        S3SimulationConfig {
            max_occlusion_samples: self.simulation.direct.occlusion_samples.unwrap_or(64) as i32,
            direct_occlusion: DirectOcclusionMode::Raycast,
            reflection_rays: self.simulation.reflections.rays.unwrap_or(4_096) as i32,
            reflection_bounces: self.simulation.reflections.bounces.unwrap_or(2) as i32,
            reflection_duration_s: self.simulation.reflections.duration_s.unwrap_or(1.0) as f32,
            reflection_effect: ReflectionEffectConfig::CONVOLUTION,
            pathing_order: self.simulation.pathing.order.unwrap_or(2) as i32,
            validate_paths: self.simulation.pathing.validation.unwrap_or(true),
            find_alternate_paths: self.simulation.pathing.alternate_paths.unwrap_or(true),
            trace_path_validation: false,
            ..S3SimulationConfig::default()
        }
    }
}

impl FixtureSource {
    pub fn initial_position(&self) -> Result<EnuVector3, String> {
        initial_position(self.position_m, self.trajectory.as_ref())
            .ok_or_else(|| format!("source {} requires a position or trajectory", self.id))
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
        assert_eq!(fixture.sources.len(), 4);
        assert!(fixture.sources[0].default_enabled);
        assert_eq!(fixture.sources[0].reference_level.db_spl, 105.0);
        assert_eq!(fixture.sources[1].asset_id, "artillery-impact");
        assert_eq!(fixture.sources[1].reference_level.db_spl, 155.0);
        assert!(!fixture.sources[1].default_enabled);
        assert_eq!(
            fixture.sources[1].initial_position().unwrap(),
            EnuVector3::new(7.5, 7.5, 1.5)
        );
        assert_eq!(fixture.sources[2].asset_id, "ff-siren");
        assert_eq!(fixture.sources[2].reference_level.db_spl, 118.0);
        assert!(!fixture.sources[2].default_enabled);
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
            fixture.sources[3].initial_position().unwrap(),
            EnuVector3::new(482.5, 292.5, 1.5)
        );
        let text = std::fs::read_to_string(fixture_path).unwrap();
        assert!(text.contains("4a614d600d4ef66a98923598a790e9b7054e4b8722af79f84fa82a0c6a0ee843"));
    }
}
