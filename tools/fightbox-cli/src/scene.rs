//! Constructs the exact backend scene from a validated fixture.
//!
//! The fixture has already been structurally validated by [`crate::fixture`].
//! This module maps it onto the public `fightbox-steam-audio` request types with
//! no shortcuts: the exact `SceneMesh`, material table, source/listener poses,
//! `ProbeVolume`, `PathBakeConfig`, and `S3SimulationConfig` come from the
//! fixture. The convenience `controlled_s3_corner`/`controlled_default` builders
//! are never used for acceptance — they remain available only for the backend
//! lane's own regression tests.

use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, EnuVector3, ListenerPose, ProbeVolume, S3BakeRequest,
    S3RenderRequest, S3SimulationConfig, S3TrajectoryRenderRequest, SceneMesh,
};

use crate::asset::ResolvedAsset;
use crate::calibrate::CalibratedSource;
use crate::fixture::{Fixture, Gate};

/// The reconstructed scene: everything the backend needs except the input PCM,
/// which is supplied by the calibrated source.
#[derive(Clone, Debug)]
pub struct FixtureScene {
    pub fixture: Fixture,
    pub mesh: SceneMesh,
    pub audio: AudioConfig,
    pub source_position_enu: EnuVector3,
    /// Validated source-local dipole model; orientation comes from its pose.
    pub source_directivity: fightbox_api::Directivity,
    pub listener: ListenerPose,
    pub calibrated: CalibratedSource,
    pub input_mono: Vec<f32>,
}

impl FixtureScene {
    /// Build the scene from a fixture and a resolved asset. This regenerates the
    /// mono PCM, derives the one source drive, and constructs the exact mesh.
    pub fn build(fixture: Fixture, asset: &ResolvedAsset) -> Result<Self, String> {
        let (signal, analysis) = asset.regenerate_mono()?;
        let calibrated = CalibratedSource::derive_from_analysis(&fixture, &analysis)?;
        let mesh = build_mesh(&fixture)?;
        let audio = AudioConfig {
            sample_rate_hz: asset.descriptor.sample_rate_hz as i32,
            frame_size: 128,
        };
        let source_position_enu = to_enu(fixture.source.position_m);
        let source_directivity = fixture.source.directivity.to_api();
        let listener = ListenerPose {
            position_enu: to_enu(fixture.listener.position_m),
            ahead_enu: to_enu(fixture.listener.forward_enu),
            up_enu: to_enu(fixture.listener.up_enu),
        };
        // Apply the one source drive to the regenerated PCM exactly once. The
        // backend receives the calibrated source buffer; it never applies a
        // second loudness gain.
        let linear_gain = calibrated.drive.linear_gain();
        let input_mono: Vec<f32> = signal
            .samples
            .iter()
            .map(|sample| sample * linear_gain)
            .collect();
        if !input_mono.iter().all(|s| s.is_finite()) {
            return Err("calibrated source PCM is not finite after drive gain".into());
        }
        Ok(Self {
            fixture,
            mesh,
            audio,
            source_position_enu,
            source_directivity,
            listener,
            calibrated,
            input_mono,
        })
    }

    /// The S3 bake request built from the exact fixture geometry and probe volume.
    pub fn s3_bake_request(&self) -> Result<S3BakeRequest, String> {
        let probe_volume = self.probe_volume()?;
        Ok(S3BakeRequest {
            mesh: self.mesh.clone(),
            probes: probe_volume,
            elevated_probe_layers: Vec::new(),
            // Phase A path-bake defaults match the accepted fixture (point
            // visibility, 6 m range, 100 m path range). The fixture records the
            // required call and serialization; runtime visibility is bounded by
            // S3SimulationConfig below.
            pathing: Default::default(),
        })
    }

    /// The S3 simulation config from the fixture's simulation block.
    pub fn s3_simulation_config(&self) -> Result<S3SimulationConfig, String> {
        let sim = &self.fixture.simulation;
        let mut config = S3SimulationConfig {
            reflection_rays: sim.reflections.rays.unwrap_or(4_096) as i32,
            reflection_bounces: sim.reflections.bounces.unwrap_or(2) as i32,
            reflection_duration_s: sim.reflections.duration_s.unwrap_or(1.0) as f32,
            ..S3SimulationConfig::default()
        };
        // maxOrder must reserve max(reflection_order, pathing_order).
        config.reflection_order = 1;
        config.pathing_order = sim.pathing.order.unwrap_or(2) as i32;
        config.validate_paths = sim.pathing.validation.unwrap_or(true);
        config.find_alternate_paths = sim.pathing.alternate_paths.unwrap_or(true);
        // Collect probe-segment validation evidence through the SDK's synchronous
        // visualization callback whenever validation is enabled. The segments are
        // the authority for "validate=true zero rejected segments".
        config.trace_path_validation = config.validate_paths;
        // Canonical S3 direct occlusion is `DirectOcclusionMode::Raycast` with
        // `max_occlusion_samples = 64` reserved as simulator capacity
        // (`IPLSimulationSettings::maxNumOcclusionSamples`). The legacy fixture
        // field `direct.occlusion_samples: 64` fixes that capacity; it does NOT
        // request Steam Audio's volumetric mode, which additionally requires a
        // positive source radius the fixture deliberately does not carry. There is
        // therefore no invented radius and no silent interpretation of a bare
        // sample count as a volumetric request: raycast is requested, raycast is
        // delivered, and the 64-sample budget is reserved for the simulator.
        config.max_occlusion_samples = sim.direct.occlusion_samples.unwrap_or(64) as i32;
        config.direct_occlusion = fightbox_steam_audio::DirectOcclusionMode::Raycast;
        Ok(config)
    }

    /// The S3 render request for the initial occluded source/listener pose.
    pub fn s3_render_request(&self) -> Result<S3RenderRequest, String> {
        Ok(S3RenderRequest {
            mesh: self.mesh.clone(),
            audio: self.audio,
            simulation: self.s3_simulation_config()?,
            source_position_enu: self.source_position_enu,
            listener: self.listener,
            input_mono: self.input_mono.clone(),
            calibration_gain: 1.0,
        })
    }

    /// The retained-session trajectory render request. The fixture's exact
    /// ordered `listener.trajectory_m` becomes the listener poses, and the
    /// calibrated source is tiled to exactly one `frame_size` block per pose
    /// (contiguous — no per-pose independent `render_s3` sessions). This is the
    /// evidence path the backend advances in a single retained context/scene/
    /// probe/simulator/source/HRTF/effect graph.
    pub fn s3_trajectory_render_request(&self) -> Result<S3TrajectoryRenderRequest, String> {
        let trajectory_m = &self.fixture.listener.trajectory_m;
        if trajectory_m.len() < 2 {
            return Err("S3 listener.trajectory_m must have at least two poses".into());
        }
        let listener_trajectory: Vec<ListenerPose> = trajectory_m
            .iter()
            .map(|p| ListenerPose {
                position_enu: to_enu(*p),
                ahead_enu: self.listener.ahead_enu,
                up_enu: self.listener.up_enu,
            })
            .collect();
        let block_size = self.audio.frame_size as usize;
        let total_frames = block_size * listener_trajectory.len();
        // Tile the calibrated source into one contiguous block per pose. The
        // input is the calibrated mono PCM; repeat it to fill the exact frame
        // budget so every pose receives a full block.
        let mut input_mono = Vec::with_capacity(total_frames);
        if self.input_mono.is_empty() {
            return Err("calibrated source PCM is empty; cannot build trajectory input".into());
        }
        for frame in 0..total_frames {
            input_mono.push(self.input_mono[frame % self.input_mono.len()]);
        }
        let mut base = self.s3_render_request()?;
        base.input_mono = input_mono;
        Ok(S3TrajectoryRenderRequest {
            base,
            listener_trajectory,
        })
    }

    fn probe_volume(&self) -> Result<ProbeVolume, String> {
        if self.fixture.gate()? != Gate::S3 {
            return Err("probe volume is only defined for S3 fixtures".into());
        }
        let sim = &self.fixture.simulation;
        let volume = sim
            .probe_volume
            .as_ref()
            .ok_or("S3 fixture missing probe_volume")?;
        let generation = sim
            .probe_generation
            .as_ref()
            .ok_or("S3 fixture missing probe_generation")?;
        Ok(ProbeVolume {
            min_enu_m: to_enu(volume.min_m),
            max_enu_m: to_enu(volume.max_m),
            spacing_m: volume.spacing_m as f32,
            height_above_floor_m: generation.height_m as f32,
        })
    }
}

/// Build the exact `SceneMesh` from a fixture's geometry. The S3 fixture's ten
/// triangles and single masonry material are validated here again so a backend
/// request is never constructed from a partial mesh.
fn build_mesh(fixture: &Fixture) -> Result<SceneMesh, String> {
    let geometry = &fixture.geometry;
    let vertices_enu_m: Vec<EnuVector3> = geometry.vertices_m.iter().copied().map(to_enu).collect();
    if vertices_enu_m.is_empty() && !geometry.triangles.is_empty() {
        return Err("geometry declares triangles without vertices".into());
    }
    let mut materials: Vec<AcousticMaterial> = Vec::new();
    let mut material_indices: Vec<i32> = Vec::with_capacity(geometry.triangles.len());
    let triangles: Vec<[i32; 3]> = geometry
        .triangles
        .iter()
        .map(|triangle| {
            let material_index = match materials.iter().position(|existing| {
                existing.absorption
                    == to_f32_array3(triangle_absorption(geometry, &triangle.material))
                    && existing.scattering == triangle_scattering(geometry, &triangle.material)
                    && existing.transmission
                        == to_f32_array3(triangle_transmission(geometry, &triangle.material))
            }) {
                Some(index) => index as i32,
                None => {
                    materials.push(AcousticMaterial {
                        absorption: to_f32_array3(triangle_absorption(
                            geometry,
                            &triangle.material,
                        )),
                        scattering: triangle_scattering(geometry, &triangle.material),
                        transmission: to_f32_array3(triangle_transmission(
                            geometry,
                            &triangle.material,
                        )),
                    });
                    (materials.len() - 1) as i32
                }
            };
            material_indices.push(material_index);
            [
                triangle.indices[0] as i32,
                triangle.indices[1] as i32,
                triangle.indices[2] as i32,
            ]
        })
        .collect();
    Ok(SceneMesh {
        vertices_enu_m,
        triangles,
        material_indices,
        materials,
    })
}

fn triangle_absorption<'a>(
    geometry: &'a crate::fixture::Geometry,
    material_name: &str,
) -> &'a [f64; 3] {
    &geometry
        .materials
        .get(material_name)
        .expect("validated fixture references a known material")
        .absorption
}

fn triangle_scattering(geometry: &crate::fixture::Geometry, material_name: &str) -> f32 {
    geometry
        .materials
        .get(material_name)
        .expect("validated fixture references a known material")
        .scattering as f32
}

fn triangle_transmission<'a>(
    geometry: &'a crate::fixture::Geometry,
    material_name: &str,
) -> &'a [f64; 3] {
    &geometry
        .materials
        .get(material_name)
        .expect("validated fixture references a known material")
        .transmission
}

fn to_f32_array3(value: &[f64; 3]) -> [f32; 3] {
    [value[0] as f32, value[1] as f32, value[2] as f32]
}

fn to_enu(vector: crate::fixture::Vec3) -> EnuVector3 {
    let [x, y, z] = vector.to_f32();
    EnuVector3::new(x, y, z)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::AssetDescriptor;
    use crate::fixture::test_fixtures;

    fn resolve_s3_pink() -> ResolvedAsset {
        AssetDescriptor::parse(include_str!(
            "../../../fixtures/assets/s3-calibrated-pink.json"
        ))
        .unwrap()
        .resolve()
        .unwrap()
    }

    #[test]
    fn builds_exact_ten_triangle_s3_mesh() {
        let scene =
            FixtureScene::build(test_fixtures::s3(), &resolve_s3_pink()).expect("scene builds");
        assert_eq!(scene.mesh.triangles.len(), 10);
        assert_eq!(scene.mesh.vertices_enu_m.len(), 10);
        assert_eq!(scene.mesh.material_indices.len(), 10);
        assert_eq!(scene.mesh.materials.len(), 1);
        // The single material is masonry at the ADR 0003 values.
        let masonry = &scene.mesh.materials[0];
        assert_eq!(masonry.absorption, [0.03, 0.05, 0.07]);
        assert_eq!(masonry.scattering, 0.1);
        assert_eq!(masonry.transmission, [0.0, 0.0, 0.0]);
        // Source/listener map to the exact ADR 0003 ENU positions.
        assert_eq!(scene.source_position_enu, EnuVector3::new(-4.0, 6.0, 1.5));
        assert_eq!(
            scene.source_directivity,
            fightbox_api::Directivity::OMNIDIRECTIONAL
        );
        assert_eq!(scene.listener.position_enu, EnuVector3::new(6.0, -4.0, 1.5));
    }

    #[test]
    fn fixture_scene_carries_present_source_directivity() {
        let mut fixture = test_fixtures::s3();
        fixture.source.directivity = crate::fixture::Directivity {
            dipole_weight: 0.7,
            dipole_power: 2.0,
        };
        let scene = FixtureScene::build(fixture, &resolve_s3_pink()).unwrap();
        assert_eq!(
            scene.source_directivity,
            fightbox_api::Directivity {
                dipole_weight: 0.7,
                dipole_power: 2.0,
            }
        );
    }

    #[test]
    fn s3_request_carries_exact_occlusion_and_order() {
        let scene = FixtureScene::build(test_fixtures::s3(), &resolve_s3_pink()).unwrap();
        let config = scene.s3_simulation_config().unwrap();
        // Canonical S3 requests raycast direct occlusion; the fixture's
        // occlusion_samples=64 is simulator capacity (max_occlusion_samples), not a
        // volumetric request. No invented radius is carried.
        assert_eq!(
            config.direct_occlusion,
            fightbox_steam_audio::DirectOcclusionMode::Raycast
        );
        assert_eq!(config.max_occlusion_samples, 64);
        assert_eq!(config.pathing_order, 2);
        assert!(config.validate_paths);
        assert!(config.find_alternate_paths);
        assert_eq!(config.reflection_rays, 4_096);
        assert_eq!(config.reflection_bounces, 2);
    }

    #[test]
    fn empty_s0_geometry_yields_empty_mesh() {
        let asset = AssetDescriptor::parse(include_str!(
            "../../../fixtures/assets/s0-calibrated-pink.json"
        ))
        .unwrap()
        .resolve()
        .unwrap();
        let scene = FixtureScene::build(test_fixtures::s0(), &asset).expect("s0 scene builds");
        assert!(scene.mesh.triangles.is_empty());
        assert!(scene.mesh.vertices_enu_m.is_empty());
    }

    #[test]
    fn s3_trajectory_request_uses_fixture_poses_and_contiguous_input() {
        let scene = FixtureScene::build(test_fixtures::s3(), &resolve_s3_pink()).unwrap();
        let request = scene.s3_trajectory_render_request().unwrap();
        // The listener trajectory must be the fixture's exact ordered poses.
        assert_eq!(
            request.listener_trajectory.len(),
            scene.fixture.listener.trajectory_m.len()
        );
        for (pose, point) in request
            .listener_trajectory
            .iter()
            .zip(&scene.fixture.listener.trajectory_m)
        {
            assert_eq!(pose.position_enu, to_enu(*point));
        }
        // The input must be exactly one frame_size block per pose (contiguous).
        let block = request.base.audio.frame_size as usize;
        assert_eq!(
            request.base.input_mono.len(),
            block * request.listener_trajectory.len()
        );
        assert!(request.base.input_mono.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn s3_trajectory_request_rejects_too_few_poses() {
        let mut fixture = test_fixtures::s3();
        // Collapse to a single pose — the trajectory builder must reject it.
        fixture.listener.trajectory_m.truncate(1);
        let scene = FixtureScene::build(fixture, &resolve_s3_pink()).unwrap();
        assert!(scene.s3_trajectory_render_request().is_err());
    }
}
