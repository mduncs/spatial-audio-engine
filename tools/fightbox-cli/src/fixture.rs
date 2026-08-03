//! Strict, serde-backed parsing of the controlled Phase A fixture contract.
//!
//! The frozen Phase A containers mirror `fixtures/fixture.schema.json` and deny
//! unknown fields. This parser also owns the backward-compatible optional
//! `source.directivity` and `source.extent` extensions; their nested shapes and
//! numeric ranges are just as strict. The structural `const` constraints the
//! frozen schema expresses (schema version, coordinate frame, kernel, gate,
//! source mode, probe/path-bake shape) are checked again here after
//! deserialization so a wrong fixture fails with a specific message rather than
//! a silent backend default. NaN/infinity cannot appear in parsed JSON text, but
//! finite reference levels and source extent dimensions are validated too.
//!
//! This layer only parses and validates; it builds no SDK handles and renders
//! nothing. Backend construction lives in [`crate::scene`].

use serde::{Deserialize, Serialize};

use crate::schema::FIXTURE;
use fightbox_api::{ExtentDescriptor, ExtentError};

/// A 3-component vector in local ENU metres, read from a JSON array of length 3.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vec3(
    /// `[x, y, z]`. Serializes natively as a JSON array; deserialization rejects
    /// non-finite components even though JSON text cannot express NaN/Infinity.
    #[serde(deserialize_with = "deserialize_finite_array3")]
    pub [f64; 3],
);

impl Vec3 {
    /// `(x, y, z)` as `f32` for backend construction.
    #[must_use]
    pub fn to_f32(self) -> [f32; 3] {
        [self.0[0] as f32, self.0[1] as f32, self.0[2] as f32]
    }
    #[must_use]
    pub fn x(self) -> f64 {
        self.0[0]
    }
    #[must_use]
    pub fn y(self) -> f64 {
        self.0[1]
    }
    #[must_use]
    pub fn z(self) -> f64 {
        self.0[2]
    }
}

fn deserialize_finite_array3<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<[f64; 3], D::Error> {
    let array = <[f64; 3]>::deserialize(deserializer)?;
    if !array.iter().all(|value| value.is_finite()) {
        return Err(serde::de::Error::custom("vec3 component must be finite"));
    }
    Ok(array)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinateFrame {
    pub name: String,
    pub units: String,
    pub axes: String,
    pub steam_audio_mapping: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kernel {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceLevel {
    pub mode: String,
    pub db_spl: f64,
}

/// Optional per-source Steam Audio dipole model in fixture JSON.
///
/// Weight blends omni to dipole, power sharpens the lobe, and the source pose's
/// forward axis supplies its orientation. The absent-field default is exactly
/// the legacy omni model.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directivity {
    pub dipole_weight: f64,
    pub dipole_power: f64,
}

impl Directivity {
    fn validate(self) -> Result<(), String> {
        if !self.dipole_weight.is_finite() {
            return Err("source.directivity.dipole_weight must be finite".into());
        }
        if !(f64::from(fightbox_api::Directivity::MIN_DIPOLE_WEIGHT)
            ..=f64::from(fightbox_api::Directivity::MAX_DIPOLE_WEIGHT))
            .contains(&self.dipole_weight)
        {
            return Err("source.directivity.dipole_weight must be in [0,1]".into());
        }
        if !self.dipole_power.is_finite() {
            return Err("source.directivity.dipole_power must be finite".into());
        }
        if !(f64::from(fightbox_api::Directivity::MIN_DIPOLE_POWER)
            ..=f64::from(fightbox_api::Directivity::MAX_DIPOLE_POWER))
            .contains(&self.dipole_power)
        {
            return Err("source.directivity.dipole_power must be in [0.25,16]".into());
        }
        Ok(())
    }

    /// Narrows a validated fixture descriptor to the domain API type.
    #[must_use]
    pub fn to_api(self) -> fightbox_api::Directivity {
        fightbox_api::Directivity {
            dipole_weight: self.dipole_weight as f32,
            dipole_power: self.dipole_power as f32,
        }
    }
}

impl Default for Directivity {
    fn default() -> Self {
        Self {
            dipole_weight: 0.0,
            dipole_power: 1.0,
        }
    }
}

/// Closed fixture JSON representation of [`ExtentDescriptor`].
///
/// Struct-style variants, including the fieldless `point` variant, let serde's
/// `deny_unknown_fields` enforce the same exact-field contract as the schema.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExtentWire {
    Point {},
    MultiPoint { count: u8 },
    LineSegment { length_m: f64 },
    StereoImage { width_m: f64 },
}

impl From<ExtentWire> for ExtentDescriptor {
    fn from(value: ExtentWire) -> Self {
        match value {
            ExtentWire::Point {} => Self::Point,
            ExtentWire::MultiPoint { count } => Self::MultiPoint { count },
            ExtentWire::LineSegment { length_m } => Self::LineSegment {
                length_m: length_m as f32,
            },
            ExtentWire::StereoImage { width_m } => Self::StereoImage {
                width_m: width_m as f32,
            },
        }
    }
}

impl From<ExtentDescriptor> for ExtentWire {
    fn from(value: ExtentDescriptor) -> Self {
        match value {
            ExtentDescriptor::Point => Self::Point {},
            ExtentDescriptor::MultiPoint { count } => Self::MultiPoint { count },
            ExtentDescriptor::LineSegment { length_m } => Self::LineSegment {
                length_m: f64::from(length_m),
            },
            ExtentDescriptor::StereoImage { width_m } => Self::StereoImage {
                width_m: f64::from(width_m),
            },
        }
    }
}

mod extent_json {
    use super::{ExtentDescriptor, ExtentWire};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ExtentDescriptor, D::Error>
    where
        D: Deserializer<'de>,
    {
        ExtentWire::deserialize(deserializer).map(Into::into)
    }

    pub fn serialize<S>(value: &ExtentDescriptor, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExtentWire::from(*value).serialize(serializer)
    }
}

fn validate_extent(extent: ExtentDescriptor) -> Result<(), String> {
    extent.validate().map_err(|error| match error {
        ExtentError::EmptyMultiPoint => "source.extent.count must be >= 1".into(),
        ExtentError::NonFiniteLineLength => "source.extent.length_m must be finite".into(),
        ExtentError::NonPositiveLineLength => "source.extent.length_m must be > 0".into(),
        ExtentError::NonFiniteStereoWidth => "source.extent.width_m must be finite".into(),
        ExtentError::NonPositiveStereoWidth => "source.extent.width_m must be > 0".into(),
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    pub position_m: Vec3,
    pub reference_level: ReferenceLevel,
    pub asset_id: String,
    #[serde(default)]
    pub directivity: Directivity,
    #[serde(default, with = "extent_json")]
    pub extent: ExtentDescriptor,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub position_m: Vec3,
    pub forward_enu: Vec3,
    pub up_enu: Vec3,
    #[serde(default)]
    pub trajectory_m: Vec<Vec3>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Triangle {
    pub indices: [u32; 3],
    pub material: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcousticMaterial {
    pub absorption: [f64; 3],
    pub scattering: f64,
    pub transmission: [f64; 3],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Geometry {
    pub vertices_m: Vec<Vec3>,
    pub triangles: Vec<Triangle>,
    pub materials: std::collections::BTreeMap<String, AcousticMaterial>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectSim {
    pub distance_attenuation: bool,
    pub occlusion: bool,
    #[serde(default)]
    pub occlusion_samples: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectionSim {
    pub enabled: bool,
    #[serde(default)]
    pub rays: Option<i64>,
    #[serde(default)]
    pub bounces: Option<i64>,
    #[serde(default)]
    pub duration_s: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathingSim {
    pub enabled: bool,
    #[serde(default)]
    pub order: Option<i64>,
    #[serde(default)]
    pub validation: Option<bool>,
    #[serde(default)]
    pub alternate_paths: Option<bool>,
    #[serde(default)]
    pub runtime_order: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirAbsorption {
    pub enabled: bool,
    pub comparison: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeVolume {
    #[allow(dead_code)]
    pub r#type: String,
    pub min_m: Vec3,
    pub max_m: Vec3,
    pub spacing_m: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeGeneration {
    #[allow(dead_code)]
    pub r#type: String,
    pub height_m: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathBake {
    pub identifier: String,
    pub required_call: String,
    pub probe_batch_serialization: String,
    pub fresh_process_reload: bool,
    pub bake_order: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    pub direct: DirectSim,
    pub reflections: ReflectionSim,
    pub pathing: PathingSim,
    #[serde(default)]
    pub air_absorption: Option<AirAbsorption>,
    #[serde(default)]
    pub probe_volume: Option<ProbeVolume>,
    #[serde(default)]
    pub probe_generation: Option<ProbeGeneration>,
    #[serde(default)]
    pub path_bake: Option<PathBake>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Analytic {
    pub edge_m: Vec3,
    pub listener_to_edge_enu: Vec3,
    pub arrival_azimuth_degrees_clockwise_from_north: f64,
    pub tolerance_degrees: f64,
    #[allow(dead_code)]
    pub meaning: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedStems {
    pub required: Vec<String>,
    pub pathing_toggle_captures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub properties: Vec<String>,
    pub non_claims: Vec<String>,
    #[serde(default)]
    pub analytic: Option<Analytic>,
    #[serde(default)]
    pub stems: Option<ExpectedStems>,
}

/// The parsed controlled-fixture descriptor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub schema_version: String,
    pub fixture_id: String,
    pub gate: String,
    pub coordinate_frame: CoordinateFrame,
    pub kernel: Kernel,
    pub source: Source,
    pub listener: Listener,
    pub geometry: Geometry,
    pub simulation: Simulation,
    pub expected: Expected,
}

/// The gate kind declared by a fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    S0,
    S3,
}

impl Fixture {
    /// Parse and structurally validate a fixture from its JSON text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let fixture: Fixture =
            serde_json::from_str(text).map_err(|e| format!("invalid fixture JSON ({e})"))?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != FIXTURE {
            return Err(format!(
                "schema_version must be {FIXTURE}, got {}",
                self.schema_version
            ));
        }
        let frame = &self.coordinate_frame;
        if frame.name != "local_enu"
            || frame.units != "meters_seconds"
            || frame.axes != "x_east_y_north_z_up"
            || frame.steam_audio_mapping != "steam_x=enu_x;steam_y=enu_z;steam_z=-enu_y"
        {
            return Err("coordinate_frame must be the ENU->Steam Audio mapping".into());
        }
        if self.kernel.name != "Steam Audio" || self.kernel.version != "4.8.1" {
            return Err("kernel must be Steam Audio 4.8.1".into());
        }
        if self.source.reference_level.mode != "SplAtOneMeter" {
            return Err("source.reference_level.mode must be SplAtOneMeter".into());
        }
        if !self.source.reference_level.db_spl.is_finite() {
            return Err("source.reference_level.db_spl must be finite".into());
        }
        self.source.directivity.validate()?;
        validate_extent(self.source.extent)?;
        // Material property ranges mirror the fixture schema.
        for (name, material) in &self.geometry.materials {
            check_material(name, material)?;
        }
        match self.gate.as_str() {
            "S0" => self.validate_s0()?,
            "S3" => self.validate_s3()?,
            other => return Err(format!("gate must be S0 or S3, got {other}")),
        }
        Ok(())
    }

    fn validate_s0(&self) -> Result<(), String> {
        let sim = &self.simulation;
        if !sim.direct.distance_attenuation {
            return Err("S0 direct.distance_attenuation must be true".into());
        }
        if sim.direct.occlusion {
            return Err("S0 direct.occlusion must be false".into());
        }
        if sim.reflections.enabled {
            return Err("S0 reflections.enabled must be false".into());
        }
        if sim.pathing.enabled {
            return Err("S0 pathing.enabled must be false".into());
        }
        let air = sim
            .air_absorption
            .as_ref()
            .ok_or("S0 requires simulation.air_absorption")?;
        if !air.enabled {
            return Err("S0 air_absorption.enabled must be true".into());
        }
        if air.comparison.is_empty() {
            return Err("S0 air_absorption.comparison must be non-empty".into());
        }
        if !self.geometry.vertices_m.is_empty() || !self.geometry.triangles.is_empty() {
            return Err("S0 geometry must be empty (free field)".into());
        }
        let trajectory = &self.listener.trajectory_m;
        if trajectory.len() < 2 {
            return Err("S0 listener.trajectory_m must have at least two points".into());
        }
        let source = self.source.position_m;
        let mut prev = f64::INFINITY;
        for point in trajectory {
            let distance = distance(source, *point);
            if distance <= 0.0 {
                return Err("S0 trajectory point must not coincide with the source".into());
            }
            if distance >= prev {
                return Err("S0 approach distances must be strictly decreasing".into());
            }
            prev = distance;
        }
        Ok(())
    }

    fn validate_s3(&self) -> Result<(), String> {
        let sim = &self.simulation;
        if !sim.direct.distance_attenuation || !sim.direct.occlusion {
            return Err("S3 direct.distance_attenuation and occlusion must be true".into());
        }
        if sim.direct.occlusion_samples != Some(64) {
            return Err("S3 direct.occlusion_samples must be exactly 64".into());
        }
        let reflections = sim.reflections;
        if !reflections.enabled
            || reflections.rays != Some(4_096)
            || reflections.bounces != Some(2)
            || reflections.duration_s != Some(1.0)
        {
            return Err(
                "S3 reflections contract must remain rays=4096,bounces=2,duration=1.0".into(),
            );
        }
        let pathing = &sim.pathing;
        if !pathing.enabled
            || pathing.order != Some(2)
            || pathing.validation != Some(true)
            || pathing.alternate_paths != Some(true)
        {
            return Err("S3 pathing must be order 2 with validation and alternates enabled".into());
        }
        if pathing.runtime_order != ["direct", "path", "reflections"] {
            return Err("S3 pathing.runtime_order must be [direct,path,reflections]".into());
        }
        let path_bake = sim
            .path_bake
            .as_ref()
            .ok_or("S3 requires simulation.path_bake")?;
        if path_bake.identifier != "s3-masonry-corner-path-bake-v1"
            || path_bake.required_call != "iplPathBakerBake"
            || path_bake.probe_batch_serialization != "required"
            || !path_bake.fresh_process_reload
            || path_bake.bake_order != 2
        {
            return Err("S3 path_bake serialization/reload contract has changed".into());
        }
        let probe_volume = sim
            .probe_volume
            .as_ref()
            .ok_or("S3 requires simulation.probe_volume")?;
        let probe_generation = sim
            .probe_generation
            .as_ref()
            .ok_or("S3 requires simulation.probe_generation")?;
        if probe_volume.r#type != "box" {
            return Err("S3 probe_volume.type must be box".into());
        }
        if probe_generation.r#type != "uniform_floor" || probe_generation.height_m != 1.5 {
            return Err("S3 probe_generation must be uniform_floor at 1.5 m".into());
        }
        if probe_volume.min_m.x() != -8.75
            || probe_volume.min_m.y() != -8.75
            || probe_volume.min_m.z() != 0.5
            || probe_volume.max_m.x() != 8.25
            || probe_volume.max_m.y() != 8.25
            || probe_volume.max_m.z() != 2.5
            || probe_volume.spacing_m != 1.0
        {
            return Err("S3 probe bounds must be (-8.75,-8.75,0.5)-(8.25,8.25,2.5) at 1 m".into());
        }
        // Exact 10-triangle ADR 0003 convex exterior corner.
        let expected_vertices = [
            [0.0, 0.0, 0.0],
            [10.0, 0.0, 0.0],
            [10.0, 0.0, 6.0],
            [0.0, 0.0, 6.0],
            [0.0, 10.0, 0.0],
            [0.0, 10.0, 6.0],
            [-9.0, -9.0, 0.0],
            [9.0, -9.0, 0.0],
            [9.0, 9.0, 0.0],
            [-9.0, 9.0, 0.0],
        ];
        if self.geometry.vertices_m.len() != 10 {
            return Err("S3 geometry must have exactly 10 vertices".into());
        }
        for (i, vertex) in self.geometry.vertices_m.iter().enumerate() {
            if vertex.0 != expected_vertices[i] {
                return Err(format!("S3 vertex {i} does not match the ADR 0003 corner"));
            }
        }
        if self.geometry.triangles.len() != 10 {
            return Err("S3 geometry must have exactly 10 triangles".into());
        }
        let expected_triangles = [
            [0, 1, 2],
            [0, 2, 3],
            [2, 1, 0],
            [3, 2, 0],
            [0, 4, 5],
            [0, 5, 3],
            [5, 4, 0],
            [3, 5, 0],
            [6, 7, 8],
            [6, 8, 9],
        ];
        for (i, triangle) in self.geometry.triangles.iter().enumerate() {
            if triangle.indices
                != [
                    expected_triangles[i][0] as u32,
                    expected_triangles[i][1] as u32,
                    expected_triangles[i][2] as u32,
                ]
            {
                return Err(format!(
                    "S3 triangle {i} does not match the ADR 0003 corner"
                ));
            }
            if triangle.material != "masonry" {
                return Err("S3 triangles must all use masonry".into());
            }
        }
        if self.geometry.materials.len() != 1 || !self.geometry.materials.contains_key("masonry") {
            return Err("S3 materials must be exactly {masonry}".into());
        }
        // Source/listener outside adjacent façades (ADR 0003).
        let source = self.source.position_m;
        let listener = self.listener.position_m;
        if source.x() != -4.0 || source.y() != 6.0 || source.z() != 1.5 {
            return Err("S3 source must be (-4,6,1.5)".into());
        }
        if listener.x() != 6.0 || listener.y() != -4.0 || listener.z() != 1.5 {
            return Err("S3 listener must be (6,-4,1.5)".into());
        }
        if self.listener.forward_enu != Vec3([0.0, 1.0, 0.0])
            || self.listener.up_enu != Vec3([0.0, 0.0, 1.0])
        {
            return Err("S3 listener forward/up must remain [0,1,0]/[0,0,1]".into());
        }
        let analytic = self
            .expected
            .analytic
            .as_ref()
            .ok_or("S3 requires expected.analytic")?;
        if analytic.edge_m != Vec3([0.0, 0.0, 1.5]) {
            return Err("S3 analytic.edge_m must be (0,0,1.5)".into());
        }
        if analytic.listener_to_edge_enu != Vec3([-6.0, 4.0, 0.0]) {
            return Err("S3 analytic.listener_to_edge_enu must be (-6,4,0)".into());
        }
        if (analytic.arrival_azimuth_degrees_clockwise_from_north - 303.690_068).abs() > 1.0e-6 {
            return Err("S3 analytic arrival azimuth must be 303.690068 degrees".into());
        }
        if analytic.tolerance_degrees != 15.0 {
            return Err("S3 analytic tolerance must be 15 degrees".into());
        }
        let stems = self
            .expected
            .stems
            .as_ref()
            .ok_or("S3 requires expected.stems")?;
        if stems.required != ["direct", "path", "reflections"] {
            return Err("S3 required stems must be [direct,path,reflections]".into());
        }
        if stems.pathing_toggle_captures != ["on", "off"] {
            return Err("S3 pathing toggles must be [on,off]".into());
        }
        Ok(())
    }

    /// The gate, narrowed from the validated string.
    pub fn gate(&self) -> Result<Gate, String> {
        match self.gate.as_str() {
            "S0" => Ok(Gate::S0),
            "S3" => Ok(Gate::S3),
            other => Err(format!("gate must be S0 or S3, got {other}")),
        }
    }
}

fn check_material(name: &str, material: &AcousticMaterial) -> Result<(), String> {
    for v in material.absorption {
        if !(0.0..=1.0).contains(&v) {
            return Err(format!("material {name} absorption out of [0,1]"));
        }
    }
    if !(0.0..=1.0).contains(&material.scattering) {
        return Err(format!("material {name} scattering out of [0,1]"));
    }
    for v in material.transmission {
        if !(0.0..=1.0).contains(&v) {
            return Err(format!("material {name} transmission out of [0,1]"));
        }
    }
    Ok(())
}

/// Euclidean distance between two fixture vectors.
pub fn distance(a: Vec3, b: Vec3) -> f64 {
    let dx = a.x() - b.x();
    let dy = a.y() - b.y();
    let dz = a.z() - b.z();
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    //! In-repo fixture text so unit tests stay portable (no filesystem access).
    use super::Fixture;

    pub const S0: &str = include_str!("../../../fixtures/s0-free-field/fixture.json");
    pub const S3: &str = include_str!("../../../fixtures/s3-corner/fixture.json");

    pub fn s0() -> Fixture {
        Fixture::parse(S0).expect("s0 fixture must parse")
    }
    pub fn s3() -> Fixture {
        Fixture::parse(S3).expect("s3 fixture must parse")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_SCHEMA: &str = include_str!("../../../fixtures/fixture.schema.json");
    const S6A_SCHEMA: &str = include_str!("../../../fixtures/s6a.schema.json");
    const MEGABLOCK: &str = include_str!("../../../fixtures/city/megablock/fixture.json");

    fn schema_accepts_directivity(value: Option<&serde_json::Value>) -> bool {
        let schema: serde_json::Value =
            serde_json::from_str(FIXTURE_SCHEMA).expect("fixture schema must be valid JSON");
        let source_schema = &schema["$defs"]["source"];
        let source_required = source_schema["required"]
            .as_array()
            .expect("source.required must be an array");

        let Some(value) = value else {
            return !source_required
                .iter()
                .any(|field| field.as_str() == Some("directivity"));
        };

        let directivity_schema = &source_schema["properties"]["directivity"];
        if directivity_schema["type"] != "object" {
            return false;
        }
        let Some(object) = value.as_object() else {
            return false;
        };
        let properties = directivity_schema["properties"]
            .as_object()
            .expect("directivity.properties must be an object");
        let required = directivity_schema["required"]
            .as_array()
            .expect("directivity.required must be an array");
        if required
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|field| !object.contains_key(field))
        {
            return false;
        }
        if directivity_schema["additionalProperties"] == false
            && object.keys().any(|field| !properties.contains_key(field))
        {
            return false;
        }

        properties.iter().all(|(field, field_schema)| {
            let Some(value) = object.get(field) else {
                return true;
            };
            if field_schema["type"] != "number" || !value.is_number() {
                return false;
            }
            let Some(number) = value.as_f64() else {
                return false;
            };
            let minimum = field_schema["minimum"]
                .as_f64()
                .expect("directivity minimum must be numeric");
            let maximum = field_schema["maximum"]
                .as_f64()
                .expect("directivity maximum must be numeric");
            (minimum..=maximum).contains(&number)
        })
    }

    fn s0_with_directivity(directivity: Option<&serde_json::Value>) -> String {
        let Some(directivity) = directivity else {
            return test_fixtures::S0.to_owned();
        };
        test_fixtures::S0.replace(
            r#""asset_id": "s0-calibrated-pink""#,
            &format!(r#""asset_id": "s0-calibrated-pink", "directivity": {directivity}"#),
        )
    }

    fn schema_accepts_extent(value: Option<&serde_json::Value>) -> bool {
        schema_accepts_extent_in(FIXTURE_SCHEMA, value)
    }

    fn schema_accepts_extent_in(schema_text: &str, value: Option<&serde_json::Value>) -> bool {
        let schema: serde_json::Value =
            serde_json::from_str(schema_text).expect("fixture schema must be valid JSON");
        let source_schema = &schema["$defs"]["source"];
        let source_required = source_schema["required"]
            .as_array()
            .expect("source.required must be an array");

        let Some(value) = value else {
            return !source_required
                .iter()
                .any(|field| field.as_str() == Some("extent"));
        };
        if source_schema["properties"]["extent"]["$ref"] != "#/$defs/extent" {
            return false;
        }

        schema["$defs"]["extent"]["oneOf"]
            .as_array()
            .expect("extent.oneOf must be an array")
            .iter()
            .filter(|shape| extent_shape_accepts(shape, value))
            .count()
            == 1
    }

    fn extent_shape_accepts(shape: &serde_json::Value, value: &serde_json::Value) -> bool {
        if shape["type"] != "object" || shape["additionalProperties"] != false {
            return false;
        }
        let Some(object) = value.as_object() else {
            return false;
        };
        let properties = shape["properties"]
            .as_object()
            .expect("extent shape properties must be an object");
        let required = shape["required"]
            .as_array()
            .expect("extent shape required must be an array");
        if required
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|field| !object.contains_key(field))
            || object.keys().any(|field| !properties.contains_key(field))
        {
            return false;
        }

        properties.iter().all(|(field, field_schema)| {
            let Some(value) = object.get(field) else {
                return true;
            };
            if let Some(expected) = field_schema.get("const") {
                return value == expected;
            }
            match field_schema["type"].as_str() {
                Some("integer") => {
                    let Some(number) = value.as_u64() else {
                        return false;
                    };
                    let minimum = field_schema["minimum"]
                        .as_u64()
                        .expect("extent integer minimum must be unsigned");
                    number >= minimum
                }
                Some("number") => {
                    let Some(number) = value.as_f64() else {
                        return false;
                    };
                    let minimum = field_schema["exclusiveMinimum"]
                        .as_f64()
                        .expect("extent number minimum must be numeric");
                    number.is_finite() && number > minimum
                }
                _ => false,
            }
        })
    }

    fn s0_with_extent(extent: Option<&serde_json::Value>) -> String {
        let Some(extent) = extent else {
            return test_fixtures::S0.to_owned();
        };
        test_fixtures::S0.replace(
            r#""asset_id": "s0-calibrated-pink""#,
            &format!(r#""asset_id": "s0-calibrated-pink", "extent": {extent}"#),
        )
    }

    #[test]
    fn parses_repo_fixtures() {
        let s0 = test_fixtures::s0();
        assert_eq!(s0.gate().unwrap(), Gate::S0);
        assert_eq!(s0.source.asset_id, "s0-calibrated-pink");
        assert_eq!(s0.source.directivity, Directivity::default());
        assert_eq!(s0.source.extent, ExtentDescriptor::Point);
        assert_eq!(s0.listener.trajectory_m.len(), 6);

        let s3 = test_fixtures::s3();
        assert_eq!(s3.gate().unwrap(), Gate::S3);
        assert_eq!(s3.geometry.triangles.len(), 10);
        assert_eq!(s3.simulation.direct.occlusion_samples, Some(64));
    }

    #[test]
    fn parses_and_round_trips_present_source_directivity() {
        let text = test_fixtures::S0.replace(
            r#""asset_id": "s0-calibrated-pink""#,
            r#""asset_id": "s0-calibrated-pink", "directivity": {"dipole_weight": 0.7, "dipole_power": 2.0}"#,
        );
        let parsed = Fixture::parse(&text).unwrap();
        assert_eq!(
            parsed.source.directivity,
            Directivity {
                dipole_weight: 0.7,
                dipole_power: 2.0,
            }
        );

        let encoded = serde_json::to_string(&parsed).unwrap();
        let reparsed = Fixture::parse(&encoded).unwrap();
        assert_eq!(reparsed, parsed);
    }

    #[test]
    fn parses_and_round_trips_all_present_source_extents() {
        for (extent, expected) in [
            (
                serde_json::json!({"kind": "point"}),
                ExtentDescriptor::Point,
            ),
            (
                serde_json::json!({"kind": "multi_point", "count": 3}),
                ExtentDescriptor::MultiPoint { count: 3 },
            ),
            (
                serde_json::json!({"kind": "line_segment", "length_m": 6.0}),
                ExtentDescriptor::LineSegment { length_m: 6.0 },
            ),
            (
                serde_json::json!({"kind": "stereo_image", "width_m": 4.0}),
                ExtentDescriptor::StereoImage { width_m: 4.0 },
            ),
        ] {
            let parsed = Fixture::parse(&s0_with_extent(Some(&extent))).unwrap();
            assert_eq!(parsed.source.extent, expected);

            let encoded = serde_json::to_string(&parsed).unwrap();
            let reparsed = Fixture::parse(&encoded).unwrap();
            assert_eq!(reparsed, parsed);
        }
    }

    #[test]
    fn json_schema_and_rust_parser_have_directivity_parity() {
        for (case, directivity, expected) in [
            ("absent", None, true),
            (
                "valid mid-range",
                Some(serde_json::json!({
                    "dipole_weight": 0.7,
                    "dipole_power": 2.0,
                })),
                true,
            ),
            (
                "inclusive upper boundaries",
                Some(serde_json::json!({
                    "dipole_weight": 1.0,
                    "dipole_power": 16.0,
                })),
                true,
            ),
            (
                "weight above maximum",
                Some(serde_json::json!({
                    "dipole_weight": 1.1,
                    "dipole_power": 2.0,
                })),
                false,
            ),
            (
                "power below minimum",
                Some(serde_json::json!({
                    "dipole_weight": 0.7,
                    "dipole_power": 0.2,
                })),
                false,
            ),
            (
                "missing power",
                Some(serde_json::json!({"dipole_weight": 0.7})),
                false,
            ),
            (
                "unknown nested field",
                Some(serde_json::json!({
                    "dipole_weight": 0.7,
                    "dipole_power": 2.0,
                    "axis": "north",
                })),
                false,
            ),
        ] {
            let schema_accepts = schema_accepts_directivity(directivity.as_ref());
            let parser_accepts = Fixture::parse(&s0_with_directivity(directivity.as_ref())).is_ok();
            assert_eq!(schema_accepts, expected, "JSON Schema outcome for {case}");
            assert_eq!(parser_accepts, expected, "Rust parser outcome for {case}");
            assert_eq!(schema_accepts, parser_accepts, "parity for {case}");
        }
    }

    #[test]
    fn json_schema_and_rust_parser_have_extent_parity() {
        for (case, extent, expected) in [
            ("absent defaults to point", None, true),
            ("point", Some(serde_json::json!({"kind": "point"})), true),
            (
                "multi-point",
                Some(serde_json::json!({"kind": "multi_point", "count": 3})),
                true,
            ),
            (
                "line segment",
                Some(serde_json::json!({
                    "kind": "line_segment",
                    "length_m": 6.0,
                })),
                true,
            ),
            (
                "stereo image",
                Some(serde_json::json!({
                    "kind": "stereo_image",
                    "width_m": 4.0,
                })),
                true,
            ),
            (
                "line segment missing length",
                Some(serde_json::json!({"kind": "line_segment"})),
                false,
            ),
            (
                "point with stray count",
                Some(serde_json::json!({"kind": "point", "count": 1})),
                false,
            ),
            (
                "empty multi-point",
                Some(serde_json::json!({"kind": "multi_point", "count": 0})),
                false,
            ),
            (
                "non-positive stereo width",
                Some(serde_json::json!({
                    "kind": "stereo_image",
                    "width_m": 0.0,
                })),
                false,
            ),
            (
                "wrong dimension for line segment",
                Some(serde_json::json!({
                    "kind": "line_segment",
                    "width_m": 6.0,
                })),
                false,
            ),
        ] {
            let fixture_schema_accepts = schema_accepts_extent(extent.as_ref());
            let s6a_schema_accepts = schema_accepts_extent_in(S6A_SCHEMA, extent.as_ref());
            let parser_accepts = Fixture::parse(&s0_with_extent(extent.as_ref())).is_ok();
            assert_eq!(
                fixture_schema_accepts, expected,
                "fixture JSON Schema outcome for {case}"
            );
            assert_eq!(
                s6a_schema_accepts, expected,
                "S6a JSON Schema outcome for {case}"
            );
            assert_eq!(parser_accepts, expected, "Rust parser outcome for {case}");
            assert_eq!(
                fixture_schema_accepts, parser_accepts,
                "fixture schema/parser parity for {case}"
            );
            assert_eq!(
                s6a_schema_accepts, parser_accepts,
                "S6a schema/parser parity for {case}"
            );
        }
    }

    #[test]
    fn megablock_directivity_matches_schema_and_rust_parser_contract() {
        let fixture: serde_json::Value =
            serde_json::from_str(MEGABLOCK).expect("megablock fixture must be valid JSON");
        let sources = fixture["sources"]
            .as_array()
            .expect("megablock sources must be an array");
        let siren = sources
            .iter()
            .find(|source| source["id"] == "siren-circling-center-block")
            .expect("megablock siren source must exist");
        let directivity = &siren["directivity"];

        assert!(schema_accepts_directivity(Some(directivity)));
        let parsed: Directivity =
            serde_json::from_value(directivity.clone()).expect("directivity shape must parse");
        parsed.validate().expect("directivity range must validate");
        assert_eq!(
            parsed,
            Directivity {
                dipole_weight: 0.5,
                dipole_power: 2.0,
            }
        );
        assert!(sources.iter().all(|source| {
            source["id"] == "siren-circling-center-block" || source.get("directivity").is_none()
        }));
    }

    #[test]
    fn megablock_extent_matches_schema_and_rust_parser_contract() {
        let fixture: serde_json::Value =
            serde_json::from_str(MEGABLOCK).expect("megablock fixture must be valid JSON");
        let sources = fixture["sources"]
            .as_array()
            .expect("megablock sources must be an array");
        let artillery = sources
            .iter()
            .find(|source| source["id"] == "artillery-sw-map-corner")
            .expect("megablock artillery source must exist");
        let extent = &artillery["extent"];

        assert!(schema_accepts_extent(Some(extent)));
        let parsed = Fixture::parse(&s0_with_extent(Some(extent))).unwrap();
        assert_eq!(
            parsed.source.extent,
            ExtentDescriptor::LineSegment { length_m: 6.0 }
        );
        // Extent-bearing megablock sources: artillery plus the DShK burst-loop
        // gun added with the composed-gunfire change.
        assert!(sources.iter().all(|source| {
            source["id"] == "artillery-sw-map-corner"
                || source["id"] == "dshk-street-gun"
                || source.get("extent").is_none()
        }));
    }

    #[test]
    fn rejects_invalid_or_unknown_shaped_source_directivity() {
        for (directivity, expected) in [
            (
                r#"{"dipole_weight": 1.01, "dipole_power": 2.0}"#,
                "dipole_weight must be in [0,1]",
            ),
            (
                r#"{"dipole_weight": 0.7, "dipole_power": 0.24}"#,
                "dipole_power must be in [0.25,16]",
            ),
            (
                r#"{"dipole_weight": 0.7, "dipole_power": 2.0, "cone_angle": 30}"#,
                "unknown field `cone_angle`",
            ),
            (r#"{"dipole_weight": 0.7}"#, "missing field `dipole_power`"),
        ] {
            let text = test_fixtures::S0.replace(
                r#""asset_id": "s0-calibrated-pink""#,
                &format!(r#""asset_id": "s0-calibrated-pink", "directivity": {directivity}"#),
            );
            let error = Fixture::parse(&text).unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?} in directivity error, got: {error}"
            );
        }
    }

    #[test]
    fn rejects_invalid_or_unknown_shaped_source_extent() {
        for (extent, expected) in [
            (
                r#"{"kind": "multi_point", "count": 0}"#,
                "source.extent.count must be >= 1",
            ),
            (
                r#"{"kind": "line_segment", "length_m": 0.0}"#,
                "source.extent.length_m must be > 0",
            ),
            (
                r#"{"kind": "stereo_image", "width_m": -1.0}"#,
                "source.extent.width_m must be > 0",
            ),
            (r#"{"kind": "line_segment"}"#, "missing field `length_m`"),
            (r#"{"kind": "point", "count": 1}"#, "unknown field `count`"),
        ] {
            let text = test_fixtures::S0.replace(
                r#""asset_id": "s0-calibrated-pink""#,
                &format!(r#""asset_id": "s0-calibrated-pink", "extent": {extent}"#),
            );
            let error = Fixture::parse(&text).unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?} in extent error, got: {error}"
            );
        }
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        // Inject an unknown key immediately before the root closing brace.
        let mut text = test_fixtures::S0.trim_end().to_string();
        text.pop(); // drop the trailing '}'
        text.push_str(r#","__unexpected": true}"#);
        let error = Fixture::parse(&text).unwrap_err();
        assert!(
            error.contains("invalid fixture JSON"),
            "expected a strict parse failure, got: {error}"
        );
    }

    #[test]
    fn rejects_wrong_schema_version() {
        let bad = test_fixtures::S0.replace("fightbox.fixture.v1", "fightbox.fixture.v2");
        let error = Fixture::parse(&bad).unwrap_err();
        assert!(error.contains("schema_version"));
    }

    #[test]
    fn rejects_non_monotonic_s0_trajectory() {
        // Swap the first two trajectory points so distances are not strictly decreasing.
        let bad =
            test_fixtures::S0.replace("[100, 0, 1.5], [75, 0, 1.5]", "[75, 0, 1.5], [100, 0, 1.5]");
        let error = Fixture::parse(&bad).unwrap_err();
        assert!(error.contains("strictly decreasing"));
    }

    #[test]
    fn rejects_s3_wrong_occlusion_samples() {
        let bad =
            test_fixtures::S3.replace(r#""occlusion_samples": 64"#, r#""occlusion_samples": 32"#);
        let error = Fixture::parse(&bad).unwrap_err();
        assert!(error.contains("occlusion_samples"));
    }

    #[test]
    fn rejects_s3_wrong_triangle_count() {
        // Remove the last floor triangle to get 9 triangles.
        let bad = test_fixtures::S3.replace(r#"{"indices": [6, 8, 9], "material": "masonry"}"#, "");
        assert!(Fixture::parse(&bad).is_err());
    }

    #[test]
    fn rejects_nonfinite_vec3() {
        // Inject Infinity into the S3 source height; JSON `Infinity` is not a valid
        // JSON number, so serde_json rejects it before structural validation.
        let bad = test_fixtures::S3.replace(
            r#""position_m": [-4, 6, 1.5]"#,
            r#""position_m": [-4, 6, Infinity]"#,
        );
        assert!(Fixture::parse(&bad).is_err());
    }
}
