use std::path::Path;

use fightbox_api::{Directivity, EnuVector3, ExtentDescriptor, ExtentError, ReferenceLevel};
use fightbox_steam_audio::{
    AcousticMaterial, BakedProbeBatch, DEFAULT_OCCLUSION_SAMPLE_COUNT,
    DEFAULT_OCCLUSION_SOURCE_RADIUS_METERS, DirectOcclusionMode,
    MAX_EXTENT_OCCLUSION_RADIUS_METERS, MIN_EXTENT_OCCLUSION_RADIUS_METERS,
    PROBE_BATCH_METADATA_SCHEMA, ProbeBatchMetadata, ReflectionEffectConfig, S3SimulationConfig,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SceneMesh,
};
use fightbox_world::LoadedPackage;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
pub struct Fixture {
    #[serde(default)]
    pub fixture_id: Option<String>,
    pub sources: Vec<FixtureSource>,
    #[serde(default)]
    pub events: Vec<FixtureEvent>,
    pub listener: FixtureListener,
    pub simulation: FixtureSimulation,
}

/// Triggerable events whose source slots are still constructed at startup.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureEvent {
    BallisticShot {
        id: String,
        trigger_key: FixtureTriggerKey,
        event_sources: BallisticEventSources,
        muzzle_m: [f64; 3],
        direction_enu: [f64; 3],
        mach: f64,
        asset_id: String,
        levels: BallisticEventLevels,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixtureTriggerKey {
    Space,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BallisticEventSources {
    pub crack: BallisticEventSourceDeclaration,
    pub blast: BallisticEventSourceDeclaration,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BallisticEventSourceDeclaration {
    pub id: String,
    pub default_active: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BallisticEventLevels {
    pub blast_spl_at_one_meter_db: f64,
    pub crack_over_blast_db_at_30_m: f64,
}

impl FixtureEvent {
    pub fn ballistic_shot(&self) -> BallisticShotFixture<'_> {
        match self {
            Self::BallisticShot {
                id,
                trigger_key,
                event_sources,
                muzzle_m,
                direction_enu,
                mach,
                asset_id,
                levels,
            } => BallisticShotFixture {
                id,
                trigger_key: *trigger_key,
                event_sources,
                muzzle_m: *muzzle_m,
                direction_enu: *direction_enu,
                mach: *mach,
                asset_id,
                levels: *levels,
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BallisticShotFixture<'a> {
    pub id: &'a str,
    pub trigger_key: FixtureTriggerKey,
    pub event_sources: &'a BallisticEventSources,
    pub muzzle_m: [f64; 3],
    pub direction_enu: [f64; 3],
    pub mach: f64,
    pub asset_id: &'a str,
    pub levels: BallisticEventLevels,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FixtureSource {
    pub id: String,
    pub asset_id: String,
    pub reference_level: FixtureReferenceLevel,
    #[serde(default = "default_enabled")]
    pub default_enabled: bool,
    #[serde(default)]
    pub directivity: FixtureDirectivity,
    #[serde(default, deserialize_with = "deserialize_extent")]
    pub extent: ExtentDescriptor,
    pub position_m: Option<[f64; 3]>,
    pub trajectory: Option<Trajectory>,
}

fn default_enabled() -> bool {
    true
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

    fn parse(bytes: &[u8], source: &str) -> Result<Self, String> {
        let fixture: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid fixture {source}: {error}"))?;
        if fixture.sources.is_empty()
            || fixture.sources.len() > fightbox_runtime::MAX_ACTIVE_SOURCES
        {
            return Err("fixture must contain 1..=MAX_ACTIVE_SOURCES ordinary sources".into());
        }
        fixture.initial_listener_position()?;
        for source in &fixture.sources {
            source.initial_position()?;
            source.reference_level.validate(&source.id)?;
            source.directivity.validate(&source.id)?;
            validate_extent(source.extent, &source.id)?;
            if let Some(trajectory) = &source.trajectory {
                trajectory.validate(&format!("source {}", source.id))?;
            }
        }
        fixture.validate_events()?;
        if let Some(trajectory) = &fixture.listener.trajectory {
            trajectory.validate("listener")?;
        }
        fixture.validate_simulation()?;
        Ok(fixture)
    }

    fn validate_events(&self) -> Result<(), String> {
        if self.events.len() > 1 {
            return Err("workbench supports at most one ballistic_shot event".into());
        }
        let mut ids = self
            .sources
            .iter()
            .map(|source| source.id.as_str())
            .collect::<Vec<_>>();
        for event in &self.events {
            let shot = event.ballistic_shot();
            if shot.id.trim().is_empty() || shot.asset_id.trim().is_empty() {
                return Err("ballistic_shot id and asset_id must be non-empty".into());
            }
            if !shot.mach.is_finite() || shot.mach <= 1.0 {
                return Err("ballistic_shot mach must be finite and > 1".into());
            }
            if shot
                .muzzle_m
                .into_iter()
                .chain(shot.direction_enu)
                .any(|component| !component.is_finite() || !(component as f32).is_finite())
            {
                return Err("ballistic_shot muzzle_m and direction_enu must be finite".into());
            }
            let direction_energy = shot
                .direction_enu
                .into_iter()
                .map(|component| component * component)
                .sum::<f64>();
            if direction_energy <= 1.0e-12 {
                return Err("ballistic_shot direction_enu must be non-zero".into());
            }
            if !shot.levels.blast_spl_at_one_meter_db.is_finite()
                || !shot.levels.crack_over_blast_db_at_30_m.is_finite()
            {
                return Err("ballistic_shot levels must be finite".into());
            }
            for source in [&shot.event_sources.crack, &shot.event_sources.blast] {
                if source.id.trim().is_empty() {
                    return Err("ballistic_shot event source ids must be non-empty".into());
                }
                if source.default_active {
                    return Err("ballistic_shot event sources must be default_active=false".into());
                }
                if ids.contains(&source.id.as_str()) {
                    return Err(format!("duplicate fixture source id {}", source.id));
                }
                ids.push(&source.id);
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn declared_source_count(&self) -> usize {
        (self.sources.len() + self.events.len() * 2).min(fightbox_runtime::MAX_ACTIVE_SOURCES)
    }

    #[must_use]
    pub fn event_requires_transient_rebuild(&self) -> bool {
        !self.events.is_empty() && self.sources.len() + 2 > fightbox_runtime::MAX_ACTIVE_SOURCES
    }

    pub fn initial_listener_position(&self) -> Result<EnuVector3, String> {
        initial_position(self.listener.position_m, self.listener.trajectory.as_ref())
            .ok_or_else(|| "listener requires a position or non-empty trajectory".into())
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
        assert_eq!(
            fixture.sources[0].directivity,
            FixtureDirectivity::default()
        );
        assert_eq!(fixture.sources[0].extent, ExtentDescriptor::Point);
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
        assert_eq!(fixture.sources.len(), 4);
        assert_eq!(fixture.declared_source_count(), 6);
        assert_eq!(fixture.events.len(), 1);
        let shot = fixture.events[0].ballistic_shot();
        assert_eq!(shot.id, "megablock-supersonic-shot");
        assert_eq!(shot.trigger_key, FixtureTriggerKey::Space);
        assert_eq!(shot.muzzle_m, [30.0, 288.0, 1.5]);
        assert_eq!(shot.direction_enu, [1.0, 0.0, 0.0]);
        assert_eq!(shot.mach, 2.5);
        assert_eq!(shot.asset_id, "artillery-impact");
        assert_eq!(shot.levels.blast_spl_at_one_meter_db, 155.0);
        assert_eq!(shot.levels.crack_over_blast_db_at_30_m, 3.0);
        assert_eq!(shot.event_sources.crack.id, "megablock-shot-crack");
        assert!(!shot.event_sources.crack.default_active);
        assert_eq!(shot.event_sources.blast.id, "megablock-shot-blast");
        assert!(!shot.event_sources.blast.default_active);
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
        assert!(fixture.sources[1].default_enabled);
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
        assert!(fixture.sources[2].default_enabled);
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
        assert!(fixture.sources[3].default_enabled);
        assert_eq!(
            fixture.sources[3].directivity,
            FixtureDirectivity::default()
        );
        assert_eq!(fixture.sources[3].extent, ExtentDescriptor::Point);
        assert_eq!(
            fixture.sources[3].initial_position().unwrap(),
            EnuVector3::new(482.5, 292.5, 60.0)
        );
        let text = std::fs::read_to_string(fixture_path).unwrap();
        assert!(text.contains("4a614d600d4ef66a98923598a790e9b7054e4b8722af79f84fa82a0c6a0ee843"));
    }

    #[test]
    fn checkpoint_fixture_matches_the_approved_scene_contract() {
        let fixture = Fixture::read(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/checkpoint/fixture.json"),
        )
        .unwrap();
        assert_eq!(fixture.fixture_id.as_deref(), Some("checkpoint-block"));
        assert_eq!(fixture.sources.len(), 8);
        assert_eq!(fixture.events.len(), 1);
        assert_eq!(fixture.declared_source_count(), 8);
        assert!(fixture.event_requires_transient_rebuild());
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
            ("ural-truck-idle-north", "squad-ural-idle", 84.0),
            ("generator-parallel-street", "squad-generator-diesel", 84.0),
            ("burning-car-west-leg", "squad-fire-car", 85.0),
            ("building-fire-far-east", "squad-fire-building-large", 96.0),
            ("fob-radio-checkpoint", "squad-fob-radio-static", 70.0),
            ("camo-net-flap-checkpoint", "squad-camo-tent-flap", 65.0),
            ("mi8-orbit", "squad-mi8-rotor-close", 126.0),
        ];
        for (source, (id, asset_id, spl)) in fixture.sources.iter().zip(expected) {
            assert_eq!(source.id, id);
            assert_eq!(source.asset_id, asset_id);
            assert_eq!(source.reference_level.db_spl, spl);
        }
        assert_eq!(
            fixture.sources[0].extent,
            ExtentDescriptor::LineSegment { length_m: 8.0 }
        );
        assert_eq!(
            fixture.sources[4].extent,
            ExtentDescriptor::LineSegment { length_m: 12.0 }
        );
        assert_eq!(fixture.sources[5].extent, ExtentDescriptor::Point);
        let orbit = fixture.sources[7].trajectory.as_ref().unwrap();
        assert_eq!(orbit.speed_mps, 30.0);
        assert_eq!(orbit.max_speed_mps, Some(30.0));
        assert_eq!(
            orbit.waypoints_m,
            vec![
                [102.5, 102.5, 55.0],
                [482.5, 102.5, 55.0],
                [482.5, 482.5, 55.0],
                [102.5, 482.5, 55.0],
            ]
        );

        let shot = fixture.events[0].ballistic_shot();
        assert_eq!(shot.id, "checkpoint-m2-shot");
        assert_eq!(shot.muzzle_m, [289.0, 102.5, 2.0]);
        assert_eq!(shot.direction_enu, [0.0, 1.0, 0.0]);
        assert_eq!(shot.mach, 2.6);
        assert_eq!(shot.asset_id, "squad-m2-blast");
        assert_eq!(shot.levels.blast_spl_at_one_meter_db, 155.0);
        assert_eq!(shot.levels.crack_over_blast_db_at_30_m, 3.0);
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
}
