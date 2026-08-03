use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke};
use fightbox_api::{
    EngineConfig, EnuVector3, ListenerState, OutputSafetyConfig, Pose, SceneCalibration, SourceId,
    SourceProfile,
};
use fightbox_runtime::backend::{SimulationUpdate, SourceMotion};
use fightbox_runtime::{
    BlockProcessor, OutputSafetyController, OutputSafetyPublication, OutputSafetyReader,
    ProcessBlock, PropagationSnapshot, RenderError, RuntimeGraph, SafetyTelemetry,
    SimulationCadences, SimulationWorker, SnapshotPublication, SnapshotReader, SnapshotWriter,
    SourcePropagation,
};
use fightbox_steam_audio::{
    AudioConfig, BakedProbeBatch, DirectOcclusionMode, MultiSourceDescriptor, S3SimulationConfig,
    SceneMesh, StageOutputGainControl, StageOutputGains, build_multi_source_session,
};
use fightbox_world::{AcousticMesh, LoadedPackage, read_package};

use crate::LaunchArgs;
use crate::acoustic_state::{
    AcousticTelemetry, AcousticTelemetryTap, BadgeTone, ProbeCoverageQuery, SourceAcousticInputs,
    SourceAcousticState, path_diagnostics_text, probe_text, probe_tone, quality_text, quality_tone,
    stage_chips,
};
use crate::asset::{PreparedAsset, load_asset};
use crate::capture::{
    BakeProvenance, BrowserScan, CaptureBrowserEntry, CaptureController, CaptureDraft,
    CaptureEndStats, CaptureEngineConfig, CaptureQualitySettings, CaptureSourceState, CaptureTap,
    WorldPackageProvenance, default_capture_root, git_identity, json_string_field,
    reveal_in_finder, scan_capture_bundles, sha256_file, utc_timestamp_now,
};
use crate::fixture::{
    Fixture, Trajectory, VisibilityRangeAdoption, load_baked, occlusion_mode_for_extent, scene_mesh,
};
use crate::mix_defaults::{
    MAX_MONITOR_GAIN_DB, MAX_SOURCE_OFFSET_DB, MIN_MONITOR_GAIN_DB, MIN_SOURCE_OFFSET_DB,
    MixDefaults, SourceHeightDefault, SourceMixDefault, clamp_source_offset_db,
};
use crate::pose::{ListenerControl, PoseMailbox};

const BLOCK_SIZE: u32 = 128;
const SAMPLE_RATE: u32 = 48_000;
const YAW_RADIANS_PER_POINT: f32 = 0.008;
const DEFAULT_AUTOPILOT_SPEED_MPS: f32 = 6.0;
const METER_WINDOW_SECONDS: f32 = 0.5;
const FIRST_PERSON_VERTICAL_FOV_RADIANS: f32 = 70.0_f32.to_radians();
const FIRST_PERSON_NEAR_M: f32 = 0.1;
/// Clearance above the tallest mesh vertex for the raised source-height option.
/// The selector label quotes this figure, so the two are asserted to agree.
const ROOFLINE_CLEARANCE_M: f32 = 3.0;
const ARTILLERY_ASSET_ID: &str = "artillery-impact";
const ARTILLERY_RETRIGGER_SECONDS: u32 = 3;
const PICTURE_IN_PICTURE_MARGIN: f32 = 14.0;

#[derive(Clone)]
struct SceneSpec {
    path: PathBuf,
    id: String,
    fixture: Fixture,
}

impl SceneSpec {
    fn read(path: PathBuf) -> Result<Self, String> {
        let fixture = Fixture::read(&path)?;
        let id = fixture.fixture_id.clone().unwrap_or_else(|| {
            path.file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("workbench-fixture")
                .to_owned()
        });
        Ok(Self { path, id, fixture })
    }
}

fn planned_physical_source_ids(fixture: &Fixture) -> Vec<String> {
    fixture
        .sources
        .iter()
        .map(|source| source.id.clone())
        .collect()
}

#[derive(Default)]
struct SceneSlotState {
    active_ids: Vec<String>,
}

impl SceneSlotState {
    fn replace(&mut self, ids: impl IntoIterator<Item = String>) {
        self.active_ids.clear();
        self.active_ids.extend(ids);
        assert!(self.active_ids.len() <= fightbox_runtime::MAX_ACTIVE_SOURCES);
    }

    fn teardown(&mut self) {
        self.active_ids.clear();
    }
}

pub struct WorkbenchApp {
    args: LaunchArgs,
    package: LoadedPackage,
    baked: BakedProbeBatch,
    scene_mesh: SceneMesh,
    assets: BTreeMap<String, PreparedAsset>,
    scenes: Vec<SceneSpec>,
    active_scene_index: usize,
    active: Option<Workbench>,
    slots: SceneSlotState,
    scene_status: Option<String>,
    startup_started: Instant,
}

impl WorkbenchApp {
    pub fn load(args: LaunchArgs, startup_started: Instant) -> Result<Self, String> {
        let phase_started = Instant::now();
        let package = read_package(&args.package)
            .map_err(|error| format!("cannot load package {}: {error}", args.package.display()))?;
        eprintln!(
            "[startup] package load: {} ms",
            phase_started.elapsed().as_millis()
        );
        let scenes = args
            .fixtures
            .iter()
            .cloned()
            .map(SceneSpec::read)
            .collect::<Result<Vec<_>, _>>()?;
        let phase_started = Instant::now();
        let asset_ids = scenes
            .iter()
            .flat_map(|scene| {
                scene
                    .fixture
                    .sources
                    .iter()
                    .map(|source| source.asset_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let assets = asset_ids
            .into_iter()
            .map(|asset_id| load_asset(&asset_id).map(|asset| (asset_id, asset)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        eprintln!(
            "[startup] prepared asset cache: {} assets in {} ms",
            assets.len(),
            phase_started.elapsed().as_millis()
        );
        let phase_started = Instant::now();
        let baked = load_baked(&args.baked, &package)?;
        eprintln!(
            "[startup] baked probes load: {} ms",
            phase_started.elapsed().as_millis()
        );
        let phase_started = Instant::now();
        let scene_mesh = scene_mesh(&package)?;
        eprintln!(
            "[startup] scene mesh preparation: {} ms",
            phase_started.elapsed().as_millis()
        );
        let active = Workbench::load_scene(
            &args,
            &scenes[0],
            &package,
            &baked,
            &scene_mesh,
            &assets,
            None,
            startup_started,
        )?;
        let mut slots = SceneSlotState::default();
        slots.replace(active.physical_source_ids());
        Ok(Self {
            args,
            package,
            baked,
            scene_mesh,
            assets,
            scenes,
            active_scene_index: 0,
            active: Some(active),
            slots,
            scene_status: None,
            startup_started,
        })
    }

    fn rebuild(&mut self, scene_index: usize, reason: &str) {
        let previous_index = self.active_scene_index;
        let previous_listener = self.active.as_ref().map(|active| active.listener);
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.can_rebuild())
        {
            self.scene_status = Some("Finish the active capture before changing scenes".into());
            return;
        }
        let stop_warning = self
            .active
            .as_ref()
            .and_then(|active| active.stop_audio().err());
        drop(self.active.take());
        self.slots.teardown();
        let build = Workbench::load_scene(
            &self.args,
            &self.scenes[scene_index],
            &self.package,
            &self.baked,
            &self.scene_mesh,
            &self.assets,
            None,
            self.startup_started,
        );
        match build {
            Ok(active) => {
                self.active_scene_index = scene_index;
                self.slots.replace(active.physical_source_ids());
                self.active = Some(active);
                let warning = stop_warning
                    .map(|warning| format!("; previous output pause warned: {warning}"))
                    .unwrap_or_default();
                self.scene_status = Some(format!(
                    "{reason}: active scene {}{warning}",
                    self.scenes[scene_index].id
                ));
            }
            Err(error) => {
                match Workbench::load_scene(
                    &self.args,
                    &self.scenes[previous_index],
                    &self.package,
                    &self.baked,
                    &self.scene_mesh,
                    &self.assets,
                    previous_listener,
                    self.startup_started,
                ) {
                    Ok(active) => {
                        self.active_scene_index = previous_index;
                        self.slots.replace(active.physical_source_ids());
                        self.active = Some(active);
                        self.scene_status = Some(format!(
                            "Could not rebuild {}: {error}; restored {}",
                            self.scenes[scene_index].id, self.scenes[previous_index].id
                        ));
                    }
                    Err(restore_error) => {
                        self.scene_status = Some(format!(
                            "Could not rebuild {}: {error}; restore also failed: {restore_error}",
                            self.scenes[scene_index].id
                        ));
                    }
                }
            }
        }
    }
}

pub struct Workbench {
    mesh: AcousticMesh,
    faces: Vec<MeshFace>,
    sources: Vec<SourceView>,
    listener: ListenerControl,
    pose_mailbox: PoseMailbox,
    simulation: SimulationWorker,
    source_motion: [SourceMotion; fightbox_runtime::MAX_ACTIVE_SOURCES],
    audio: AudioState,
    camera: Camera,
    monitor_gain_db: f32,
    output_safety_controller: OutputSafetyController,
    meter_reader: SnapshotReader<MeterReading>,
    source_mix_writer: SnapshotWriter<SourceMix>,
    stage_mix: StageMix,
    stage_output_gain_control: StageOutputGainControl,
    audio_block_reader: SnapshotReader<u64>,
    capture: CaptureController,
    capture_state: CaptureUiState,
    capture_static: CaptureStaticContext,
    capture_entries: Vec<CaptureBrowserEntry>,
    capture_warnings: Vec<String>,
    capture_status: Option<String>,
    fixture_path: PathBuf,
    mix_defaults_status: Option<String>,
    autopilot: Autopilot,
    source_height_levels: SourceHeightLevels,
    probe_coverage: ProbeCoverageQuery,
    acoustic_telemetry: SnapshotReader<AcousticTelemetry>,
    visibility_range: VisibilityRangeAdoption,
    startup_started: Instant,
    reflection_warmup_started: Instant,
    reflection_warmup_reported: bool,
    first_frame_reported: bool,
}

struct SourceView {
    id: String,
    asset_id: String,
    position: EnuVector3,
    declared_spl_at_one_meter_db: f32,
    monitor_offset_db: f32,
    enabled: bool,
    muted: bool,
    soloed: bool,
    street_height_m: f32,
    height: SourceHeight,
    trajectory: Option<SourceTrajectory>,
    acoustic: SourceAcousticState,
    occlusion_mode: DirectOcclusionMode,
}

/// The sidecar spells the raised option `above_rooves`; that token is frozen for
/// backward compatibility, so only the display label states the offset it
/// actually applies (see [`SourceHeightLevels::height_m`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceHeight {
    Street,
    Medium,
    AboveRooves,
}

impl SourceHeight {
    const ALL: [Self; 3] = [Self::Street, Self::Medium, Self::AboveRooves];

    fn label(self) -> &'static str {
        match self {
            Self::Street => "street",
            Self::Medium => "medium",
            Self::AboveRooves => "roofline +3 m",
        }
    }
}

impl From<SourceHeightDefault> for SourceHeight {
    fn from(height: SourceHeightDefault) -> Self {
        match height {
            SourceHeightDefault::Street => Self::Street,
            SourceHeightDefault::Medium => Self::Medium,
            SourceHeightDefault::AboveRooves => Self::AboveRooves,
        }
    }
}

impl From<SourceHeight> for SourceHeightDefault {
    fn from(height: SourceHeight) -> Self {
        match height {
            SourceHeight::Street => Self::Street,
            SourceHeight::Medium => Self::Medium,
            SourceHeight::AboveRooves => Self::AboveRooves,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SourceHeightLevels {
    tallest_roof_m: f32,
}

impl SourceHeightLevels {
    fn for_mesh(mesh: &AcousticMesh) -> Self {
        let tallest_roof_m = mesh
            .vertices_enu_m
            .iter()
            .map(|vertex| vertex.up_m)
            .reduce(f32::max)
            .unwrap_or_default();
        Self { tallest_roof_m }
    }

    fn height_m(self, selection: SourceHeight, street_height_m: f32) -> f32 {
        match selection {
            SourceHeight::Street => street_height_m,
            SourceHeight::Medium => self.tallest_roof_m * 0.5,
            SourceHeight::AboveRooves => self.tallest_roof_m + ROOFLINE_CLEARANCE_M,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StageMix {
    pub(crate) bypassed: [bool; 3],
    pub(crate) soloed: [bool; 3],
}

impl StageMix {
    pub(crate) const ALL_ENABLED: Self = Self {
        bypassed: [false; 3],
        soloed: [false; 3],
    };

    pub(crate) fn gains(self) -> StageOutputGains {
        let any_soloed = self
            .bypassed
            .iter()
            .zip(self.soloed)
            .any(|(bypassed, soloed)| !*bypassed && soloed);
        let enabled = std::array::from_fn::<_, 3, _>(|index| {
            f32::from(!self.bypassed[index] && (!any_soloed || self.soloed[index]))
        });
        StageOutputGains {
            direct: enabled[0],
            pathing: enabled[1],
            reflections: enabled[2],
        }
    }
}

enum CaptureUiState {
    Idle,
    Recording { bundle: PathBuf },
    Stopping,
    Finishing,
}

struct CaptureStaticContext {
    fixture_id: String,
    fixture_path: String,
    fixture_content_sha256: String,
    engine_commit: Option<String>,
    engine_dirty: Option<bool>,
    world_package: WorldPackageProvenance,
    bake: BakeProvenance,
    quality: CaptureQualitySettings,
    engine_config: CaptureEngineConfig,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SourceMix {
    enabled: [bool; fightbox_runtime::MAX_ACTIVE_SOURCES],
    muted: [bool; fightbox_runtime::MAX_ACTIVE_SOURCES],
    soloed: [bool; fightbox_runtime::MAX_ACTIVE_SOURCES],
    monitor_gains: [f32; fightbox_runtime::MAX_ACTIVE_SOURCES],
}

impl SourceMix {
    const ALL_AUDIBLE: Self = Self {
        enabled: [true; fightbox_runtime::MAX_ACTIVE_SOURCES],
        muted: [false; fightbox_runtime::MAX_ACTIVE_SOURCES],
        soloed: [false; fightbox_runtime::MAX_ACTIVE_SOURCES],
        monitor_gains: [1.0; fightbox_runtime::MAX_ACTIVE_SOURCES],
    };

    fn from_sources(sources: &[SourceView]) -> Self {
        let mut mix = Self::ALL_AUDIBLE;
        for (index, source) in sources.iter().enumerate() {
            mix.enabled[index] = source.enabled;
            mix.muted[index] = source.muted;
            mix.soloed[index] = source.soloed;
            mix.monitor_gains[index] = monitor_offset_gain(source.monitor_offset_db);
        }
        mix
    }

    fn gains(self, source_count: usize) -> [f32; fightbox_runtime::MAX_ACTIVE_SOURCES] {
        let any_soloed = self.enabled[..source_count]
            .iter()
            .zip(&self.soloed[..source_count])
            .any(|(enabled, soloed)| *enabled && *soloed);
        std::array::from_fn(|index| {
            f32::from(
                index < source_count
                    && self.enabled[index]
                    && !self.muted[index]
                    && (!any_soloed || self.soloed[index]),
            ) * self.monitor_gains[index]
        })
    }
}

fn monitor_offset_gain(offset_db: f32) -> f32 {
    10.0_f32.powf(clamp_source_offset_db(offset_db) / 20.0)
}

fn format_db_number(value: f32) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}", value)
    } else {
        format!("{value:.1}")
    }
}

fn format_level_truth(base_db: f32, offset_db: f32) -> String {
    let operator = if offset_db < 0.0 { "" } else { "+" };
    format!(
        "{} {operator}{} -> {} dB SPL",
        format_db_number(base_db),
        format_db_number(offset_db),
        format_db_number(base_db + offset_db),
    )
}

enum AudioState {
    #[cfg(feature = "live-output")]
    Live(fightbox_runtime::live::LiveOutput),
    Unavailable(String),
}

fn configure_output_safety(
    listener_position: EnuVector3,
    profiles: &[SourceProfile],
) -> Result<(OutputSafetyController, OutputSafetyReader), String> {
    let (mut controller, reader) = OutputSafetyPublication::new(OutputSafetyConfig::default())
        .map_err(|error| format!("cannot create output-safety publication: {error:?}"))?;
    controller
        .set_listener_position(listener_position)
        .map_err(|error| format!("cannot configure output-safety listener: {error:?}"))?;
    for (index, profile) in profiles.iter().enumerate() {
        controller
            .set_source(index, profile, None)
            .map_err(|error| format!("cannot configure output-safety source {index}: {error:?}"))?;
    }
    Ok((controller, reader))
}

impl Workbench {
    fn load_scene(
        args: &LaunchArgs,
        scene_spec: &SceneSpec,
        package: &LoadedPackage,
        baked: &BakedProbeBatch,
        scene_mesh: &SceneMesh,
        assets: &BTreeMap<String, PreparedAsset>,
        listener_override: Option<ListenerControl>,
        startup_started: Instant,
    ) -> Result<Self, String> {
        let fixture = &scene_spec.fixture;
        let listener = listener_override.unwrap_or_else(|| {
            ListenerControl::at(
                fixture
                    .initial_listener_position()
                    .expect("scene specifications are validated when loaded"),
                to_enu(fixture.listener.forward_enu),
            )
        });
        let initial_listener = listener.listener_state(EnuVector3::default());
        let (pose_mailbox, pose_reader) = PoseMailbox::new(initial_listener);
        let visibility_range = fixture.visibility_range_adoption();
        if visibility_range.rebaselined {
            eprintln!(
                "!!! [startup] PATH VISIBILITY RANGE RE-BASELINED: configured {:.2} m is below 2.5 x {:.2} m probe spacing ({:.2} m); adopting {:.2} m. Session telemetry and captures are flagged re-baselined.",
                visibility_range.configured_m,
                visibility_range.probe_spacing_m,
                visibility_range.minimum_for_spacing_m,
                visibility_range.effective_m,
            );
        }
        let simulation_config = fixture.simulation_config();
        let fixture_id = scene_spec.id.clone();
        let fixture_content_sha256 = sha256_file(&scene_spec.path)
            .ok_or_else(|| format!("cannot hash fixture {}", scene_spec.path.display()))?;
        let package_manifest_sha256 = sha256_file(&args.package.join("manifest.json"));
        let bake_manifest_path = args.baked.join("city-bake-manifest.json");
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (engine_commit, engine_dirty) = git_identity(&repository);
        let capture_static = CaptureStaticContext {
            fixture_id,
            fixture_path: scene_spec.path.display().to_string(),
            fixture_content_sha256,
            engine_commit,
            engine_dirty,
            world_package: WorldPackageProvenance {
                path: args.package.display().to_string(),
                package_manifest_sha256,
                mesh_content_sha256: package.manifest.mesh_content_sha256.clone(),
                materials_content_sha256: package.manifest.materials_content_sha256.clone(),
            },
            bake: BakeProvenance {
                path: args.baked.display().to_string(),
                identifier: json_string_field(&bake_manifest_path, "/schema_version"),
                bake_manifest_sha256: sha256_file(&bake_manifest_path),
                probe_batch_content_sha256: baked.metadata.content_sha256.clone(),
            },
            quality: capture_quality(simulation_config, visibility_range),
            engine_config: CaptureEngineConfig {
                sample_rate_hz: SAMPLE_RATE,
                block_size_frames: BLOCK_SIZE,
                speed_of_sound_mps: EngineConfig::default().speed_of_sound_mps,
                max_active_sources: fixture.declared_source_count() as u8,
            },
        };

        let planned_source_ids = planned_physical_source_ids(fixture);
        let runtime_source_count = planned_source_ids.len();
        if runtime_source_count > fightbox_runtime::MAX_ACTIVE_SOURCES {
            return Err(format!(
                "scene {} requires {runtime_source_count} physical source slots",
                scene_spec.id
            ));
        }
        let mut prepared_sources = Vec::with_capacity(runtime_source_count);
        let mut descriptors = Vec::with_capacity(runtime_source_count);
        let mut source_motion = [SourceMotion::default(); fightbox_runtime::MAX_ACTIVE_SOURCES];
        let mut source_views = Vec::with_capacity(runtime_source_count);
        for source in &fixture.sources {
            let index = prepared_sources.len();
            let position = source.initial_position()?;
            let trajectory = source
                .trajectory
                .as_ref()
                .map(SourceTrajectory::from_fixture)
                .transpose()?;
            let asset = assets
                .get(&source.asset_id)
                .ok_or_else(|| format!("asset cache is missing {}", source.asset_id))?
                .clone();
            let echo_impulse_class = if source.asset_id == "squad-a10-impacts" {
                fightbox_api::ImpulseClass::ArtilleryThunder
            } else {
                fightbox_api::ImpulseClass::None
            };
            let echo_profile = asset.echo_profile(source.impulsive, echo_impulse_class)?;
            let pose = Pose {
                position,
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            };
            let profile = SourceProfile {
                id: SourceId::new(&source.id),
                pose,
                reference_level: source.reference_level.to_api(),
                asset_analysis: asset.analysis,
                extent: source.extent,
                directivity: source.directivity.to_api(),
                max_speed_mps: source
                    .trajectory
                    .as_ref()
                    .map(|trajectory| {
                        trajectory.max_speed_mps.unwrap_or(trajectory.speed_mps) as f32
                    })
                    .unwrap_or(0.0),
            };
            descriptors.push(
                MultiSourceDescriptor::at(profile.pose.position)
                    .with_reference_level(profile.reference_level)
                    .with_directivity(profile.directivity)
                    .with_extent(profile.extent)
                    .with_echo_profile(echo_profile),
            );
            prepared_sources.push((profile, asset.samples));
            source_motion[index] = SourceMotion {
                active: true,
                pose,
                linear_velocity_mps: EnuVector3::default(),
            };
            source_views.push(SourceView {
                id: source.id.clone(),
                asset_id: source.asset_id.clone(),
                position,
                declared_spl_at_one_meter_db: source.reference_level.db_spl as f32,
                monitor_offset_db: 0.0,
                enabled: source.default_enabled,
                muted: false,
                soloed: false,
                street_height_m: position.up_m,
                height: SourceHeight::Street,
                trajectory,
                acoustic: SourceAcousticState::UNKNOWN,
                occlusion_mode: occlusion_mode_for_extent(simulation_config, source.extent),
            });
        }
        // User mix defaults are resolved only after calibrated profiles and
        // backend descriptors are complete. They never alter the fixture's
        // calibrated source declarations; saved heights are applied to runtime
        // positions through the same control path as a UI selection below.
        let mut monitor_gain_db = OutputSafetyConfig::DEFAULT_MONITOR_GAIN_DB;
        let mut mix_defaults_status = None;
        let mut saved_source_heights = Vec::new();
        match MixDefaults::read(&scene_spec.path) {
            Ok(Some(defaults)) => {
                let valid_source_ids = fixture.sources.iter().map(|source| source.id.clone());
                let resolved = defaults.resolve(valid_source_ids);
                monitor_gain_db = resolved.monitor_gain_db;
                for (index, source) in source_views.iter_mut().enumerate() {
                    if let Some(saved) = resolved.sources.get(&source.id) {
                        source.enabled = saved.enabled;
                        source.muted = saved.muted;
                        source.soloed = saved.soloed;
                        source.monitor_offset_db = saved.monitor_offset_db;
                        saved_source_heights.push((index, saved.height.into()));
                    }
                }
                mix_defaults_status = if resolved.ignored_source_ids.is_empty() {
                    Some("Loaded saved mix defaults".into())
                } else {
                    Some(format!(
                        "Loaded saved mix defaults; ignored unknown source ids: {}",
                        resolved.ignored_source_ids.join(", ")
                    ))
                };
            }
            Ok(None) => {}
            Err(error) => mix_defaults_status = Some(error),
        }
        let audio_config = AudioConfig {
            sample_rate_hz: SAMPLE_RATE as i32,
            frame_size: BLOCK_SIZE as i32,
        };
        let phase_started = Instant::now();
        let (runner, mut backend) = build_multi_source_session(
            scene_mesh,
            baked,
            audio_config,
            simulation_config,
            &descriptors,
        )
        .map_err(|error| format!("cannot build Steam Audio session: {error}"))?;
        let stage_output_gain_control = backend
            .take_stage_output_gain_control()
            .ok_or("Steam Audio render graph did not expose stage-gain control")?;
        eprintln!(
            "[startup] steam scene + simulator build: {} ms",
            phase_started.elapsed().as_millis()
        );
        let probe_coverage = match baked.probe_coverage() {
            Ok(coverage) => ProbeCoverageQuery::from_spheres(coverage.spheres().collect()),
            Err(error) => {
                eprintln!("[startup] probe-coverage badges unavailable: {error}");
                ProbeCoverageQuery::unavailable()
            }
        };
        let (runner, acoustic_telemetry) = AcousticTelemetryTap::new(runner);
        let initial_update = SimulationUpdate {
            listener: initial_listener,
            sources: source_motion,
        };
        let reflection_warmup_started = Instant::now();
        let simulation = SimulationWorker::new(
            Box::new(runner),
            initial_update,
            SimulationCadences::default(),
        )
        .map_err(|error| format!("cannot start simulation worker: {error:?}"))?;
        eprintln!(
            "[startup] simulation worker started: {} ms",
            reflection_warmup_started.elapsed().as_millis()
        );

        let phase_started = Instant::now();
        let propagation = PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: u64::MAX,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < prepared_sources.len(),
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        };
        let (_writer, reader) = SnapshotPublication::new(propagation);
        let engine_config = EngineConfig {
            sample_rate_hz: SAMPLE_RATE,
            block_size_frames: BLOCK_SIZE,
            max_active_sources: prepared_sources.len() as u8,
            ..EngineConfig::default()
        };
        let (mut output_safety_controller, output_safety_reader) = configure_output_safety(
            initial_listener.pose.position,
            &prepared_sources
                .iter()
                .map(|(profile, _)| profile.clone())
                .collect::<Vec<_>>(),
        )?;
        output_safety_controller
            .set_monitor_gain_db(monitor_gain_db)
            .map_err(|error| format!("cannot apply saved monitor gain: {error:?}"))?;
        let mut graph = RuntimeGraph::new_with_backend_and_output_safety(
            engine_config,
            reader,
            output_safety_reader,
            Box::new(backend),
        )
        .map_err(|error| format!("cannot create runtime graph: {error:?}"))?;
        graph.set_listener_state(initial_listener);
        for (index, (profile, _)) in prepared_sources.iter().enumerate() {
            graph
                .set_source(index, profile, SceneCalibration::default())
                .map_err(|error| format!("cannot configure source {index}: {error:?}"))?;
        }
        eprintln!(
            "[startup] runtime graph configuration: {} ms",
            phase_started.elapsed().as_millis()
        );
        let (meter_writer, meter_reader) = SnapshotPublication::new(MeterReading::SILENT);
        let initial_source_mix = SourceMix::from_sources(&source_views);
        let (source_mix_writer, source_mix_reader) = SnapshotPublication::new(initial_source_mix);
        let (audio_block_writer, audio_block_reader) = SnapshotPublication::new(0_u64);
        let capture_root = default_capture_root()?;
        let browser = scan_capture_bundles(&capture_root).unwrap_or_else(|error| BrowserScan {
            entries: vec![],
            warnings: vec![error],
        });
        let (capture, capture_tap) = CaptureController::new(capture_root);
        let processor = LateBoundProcessor::new(
            graph,
            pose_reader,
            meter_writer,
            MeterAccumulator::new(SAMPLE_RATE, BLOCK_SIZE, METER_WINDOW_SECONDS),
            audio_block_writer,
            Some(capture_tap),
        );
        let playback = source_views
            .iter()
            .map(|source| SourcePlayback::for_asset(&source.asset_id, SAMPLE_RATE))
            .collect();
        let signals = prepared_sources
            .into_iter()
            .map(|(_, samples)| samples)
            .collect();
        let phase_started = Instant::now();
        let audio = start_audio(
            processor,
            engine_config,
            signals,
            playback,
            source_mix_reader,
            args.device.as_deref(),
        );
        eprintln!(
            "[startup] audio stream initialization: {} ms",
            phase_started.elapsed().as_millis()
        );
        let phase_started = Instant::now();
        let faces = mesh_faces(&package.mesh);
        let camera = Camera::for_mesh(&package.mesh);
        let autopilot = Autopilot::for_scene(
            Bounds2::for_mesh(&package.mesh),
            fixture,
            &scene_spec.id,
            listener.position,
        );
        let source_height_levels = SourceHeightLevels::for_mesh(&package.mesh);
        eprintln!(
            "[startup] workbench view preparation: {} ms",
            phase_started.elapsed().as_millis()
        );
        let mut workbench = Self {
            mesh: package.mesh.clone(),
            faces,
            sources: source_views,
            listener,
            pose_mailbox,
            simulation,
            source_motion,
            audio,
            camera,
            monitor_gain_db,
            output_safety_controller,
            meter_reader,
            source_mix_writer,
            stage_mix: StageMix::ALL_ENABLED,
            stage_output_gain_control,
            audio_block_reader,
            capture,
            capture_state: CaptureUiState::Idle,
            capture_static,
            capture_entries: browser.entries,
            capture_warnings: browser.warnings,
            capture_status: None,
            fixture_path: scene_spec.path.clone(),
            mix_defaults_status,
            autopilot,
            source_height_levels,
            probe_coverage,
            acoustic_telemetry,
            visibility_range,
            startup_started,
            reflection_warmup_started,
            reflection_warmup_reported: false,
            first_frame_reported: false,
        };
        for (index, height) in saved_source_heights {
            workbench.apply_source_height(index, height);
        }
        debug_assert_eq!(workbench.physical_source_ids(), planned_source_ids);
        Ok(workbench)
    }

    fn physical_source_ids(&self) -> Vec<String> {
        self.sources
            .iter()
            .map(|source| source.id.clone())
            .collect()
    }

    fn can_rebuild(&self) -> bool {
        matches!(self.capture_state, CaptureUiState::Idle)
    }

    fn stop_audio(&self) -> Result<(), String> {
        match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(output) => output
                .stop()
                .map_err(|error| format!("cannot pause output for scene rebuild: {error:?}")),
            AudioState::Unavailable(_) => Ok(()),
        }
    }

    fn update_source_motion(&mut self) {
        let elapsed_blocks = self.audio_block_reader.read();
        for index in 0..self.sources.len() {
            let Some((mut sample, speed_mps)) =
                self.sources[index].trajectory.as_ref().map(|trajectory| {
                    (
                        trajectory.sample_at_block(elapsed_blocks),
                        trajectory.speed_mps,
                    )
                })
            else {
                continue;
            };
            sample.position.up_m = self.source_height_levels.height_m(
                self.sources[index].height,
                self.sources[index].street_height_m,
            );
            self.update_source_position(index, sample.position);
            self.source_motion[index].pose.forward = sample.direction;
            self.source_motion[index].linear_velocity_mps = scale(sample.direction, speed_mps);
        }
    }

    /// Re-resolves every source's badge row from the latest simulation
    /// publication and the source's current position. Control-tick only.
    fn refresh_acoustic_state(&mut self) {
        let telemetry = self.acoustic_telemetry.read();
        let listener_probes = self.probe_coverage.coverage(self.listener.position);
        let stage_gains = self.stage_mix.gains();
        let mix_gains = SourceMix::from_sources(&self.sources).gains(self.sources.len());
        for (index, source) in self.sources.iter_mut().enumerate() {
            let inputs = SourceAcousticInputs {
                source_probes: self.probe_coverage.coverage(source.position),
                listener_probes,
                audible_in_mix: mix_gains[index] > 0.0,
                stage_gains,
            };
            source.acoustic = SourceAcousticState::evaluate(inputs, telemetry, index);
        }
    }

    fn update_source_position(&mut self, index: usize, position: EnuVector3) {
        self.sources[index].position = position;
        self.source_motion[index].pose.position = position;
        self.output_safety_controller
            .set_source_position(index, position)
            .expect("workbench source positions remain finite");
    }

    fn apply_source_height(&mut self, index: usize, selection: SourceHeight) {
        self.sources[index].height = selection;
        let mut position = self.sources[index].position;
        position.up_m = self
            .source_height_levels
            .height_m(selection, self.sources[index].street_height_m);
        self.update_source_position(index, position);
    }

    fn update_control(&mut self, ctx: &egui::Context, drag_delta_x: f32) {
        if drag_delta_x != 0.0 && !self.autopilot.enabled {
            self.listener.turn(drag_delta_x * YAW_RADIANS_PER_POINT);
        }
        let (forward, right, sprinting, delta_seconds) = ctx.input(|input| {
            (
                axis(input, egui::Key::W, egui::Key::S),
                axis(input, egui::Key::D, egui::Key::A),
                input.modifiers.shift,
                input.stable_dt.min(0.1),
            )
        });
        if self.autopilot.enabled && (forward != 0.0 || right != 0.0) {
            self.autopilot.enabled = false;
        }
        let velocity = if self.autopilot.enabled {
            let sample = self.autopilot.advance(delta_seconds);
            self.listener.position = EnuVector3::new(
                sample.position[0],
                sample.position[1],
                self.listener.position.up_m,
            );
            self.listener.yaw_radians = sample.direction[0].atan2(sample.direction[1]);
            EnuVector3::new(
                sample.direction[0] * self.autopilot.speed_mps,
                sample.direction[1] * self.autopilot.speed_mps,
                0.0,
            )
        } else {
            self.listener.walk(forward, right, sprinting, delta_seconds)
        };
        let listener = self.listener.listener_state(velocity);
        self.output_safety_controller
            .set_listener_position(listener.pose.position)
            .expect("workbench listener controls remain finite");
        self.pose_mailbox.publish(listener);
        self.simulation.publish_update(SimulationUpdate {
            listener,
            sources: self.source_motion,
        });
    }

    fn draw_scene(&self, painter: &egui::Painter, rect: Rect) {
        let painter = painter.with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(13, 18, 24));
        let mut faces = self
            .faces
            .iter()
            .filter_map(|face| {
                project_face(
                    &self.mesh,
                    *face,
                    rect,
                    |point| self.camera.camera_point(point),
                    |point, rect| self.camera.screen_point(point, rect),
                )
            })
            .collect::<Vec<_>>();
        paint_faces(&painter, &mut faces);
        for source in &self.sources {
            self.draw_map_trajectory(&painter, rect, source);
        }
        for source in &self.sources {
            if let Some(point) = self.camera.project(source.position, rect) {
                painter.circle_filled(point, 5.0, Color32::from_rgb(255, 174, 66));
                painter.text(
                    point + egui::vec2(8.0, -8.0),
                    egui::Align2::LEFT_BOTTOM,
                    &source.id,
                    egui::FontId::monospace(11.0),
                    Color32::from_rgb(255, 213, 146),
                );
            }
        }
        let listener = self.listener.position;
        let arrow_end = add(listener, scale(self.listener.forward(), 4.0));
        if let (Some(origin), Some(end)) = (
            self.camera.project(listener, rect),
            self.camera.project(arrow_end, rect),
        ) {
            painter.circle_filled(origin, 5.0, Color32::from_rgb(64, 211, 176));
            painter.arrow(
                origin,
                end - origin,
                Stroke::new(2.5, Color32::from_rgb(64, 211, 176)),
            );
        }
    }

    fn draw_first_person(&self, painter: &egui::Painter, rect: Rect) {
        let painter = painter.with_clip_rect(rect);
        painter.rect_filled(rect, 3.0, Color32::from_rgb(8, 12, 17));
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0, Color32::from_rgb(105, 136, 153)),
            egui::StrokeKind::Inside,
        );
        let projection = FirstPersonProjection::new(
            self.listener.position,
            self.listener.yaw_radians,
            FIRST_PERSON_VERTICAL_FOV_RADIANS,
            FIRST_PERSON_NEAR_M,
        );
        let mut faces = self
            .faces
            .iter()
            .filter_map(|face| {
                project_face(
                    &self.mesh,
                    *face,
                    rect,
                    |point| projection.camera_point(point),
                    |point, rect| projection.screen_point(point, rect),
                )
            })
            .collect::<Vec<_>>();
        paint_faces(&painter, &mut faces);
        for source in &self.sources {
            self.draw_first_person_trajectory(&painter, rect, projection, source);
        }
        for source in &self.sources {
            if let Some((point, distance)) = projection.project_point(source.position, rect) {
                let radius = (32.0 / distance.max(1.0)).clamp(2.5, 10.0);
                painter.circle_filled(point, radius, Color32::from_rgb(255, 174, 66));
                painter.text(
                    point + egui::vec2(radius + 3.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &source.id,
                    egui::FontId::monospace(10.0),
                    Color32::from_rgb(255, 213, 146),
                );
            }
        }
        painter.text(
            rect.left_top() + egui::vec2(8.0, 7.0),
            egui::Align2::LEFT_TOP,
            "LISTENER VIEW",
            egui::FontId::monospace(10.0),
            Color32::from_rgb(142, 173, 188),
        );
    }

    fn draw_map_trajectory(&self, painter: &egui::Painter, rect: Rect, source: &SourceView) {
        let Some(trajectory) = &source.trajectory else {
            return;
        };
        for [a, b] in trajectory_segments_at_height(trajectory, source.position.up_m) {
            if let (Some(a), Some(b)) = (self.camera.project(a, rect), self.camera.project(b, rect))
            {
                painter.line_segment([a, b], Stroke::new(1.5, Color32::from_rgb(222, 143, 54)));
            }
        }
    }

    fn draw_first_person_trajectory(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        projection: FirstPersonProjection,
        source: &SourceView,
    ) {
        let Some(trajectory) = &source.trajectory else {
            return;
        };
        for [a, b] in trajectory_segments_at_height(trajectory, source.position.up_m) {
            if let Some(points) = projection.project_segment(a, b, rect) {
                painter.line_segment(points, Stroke::new(1.5, Color32::from_rgb(222, 143, 54)));
            }
        }
    }

    fn capture_draft(&self) -> CaptureDraft {
        CaptureDraft {
            started_utc: utc_timestamp_now(),
            fixture_id: self.capture_static.fixture_id.clone(),
            fixture_path: self.capture_static.fixture_path.clone(),
            fixture_content_sha256: self.capture_static.fixture_content_sha256.clone(),
            engine_commit: self.capture_static.engine_commit.clone(),
            engine_dirty: self.capture_static.engine_dirty,
            world_package: self.capture_static.world_package.clone(),
            bake: self.capture_static.bake.clone(),
            sources: self
                .sources
                .iter()
                .map(|source| {
                    let (occlusion_mode, occlusion_radius_m, occlusion_samples) =
                        match source.occlusion_mode {
                            DirectOcclusionMode::Raycast => ("raycast".into(), None, None),
                            DirectOcclusionMode::Volumetric {
                                radius_m,
                                sample_count,
                            } => ("volumetric".into(), Some(radius_m), Some(sample_count)),
                        };
                    CaptureSourceState {
                        id: source.id.clone(),
                        asset_id: source.asset_id.clone(),
                        reference_level_mode: "SplAtOneMeter".into(),
                        reference_level_db_spl: source.declared_spl_at_one_meter_db,
                        governor_physically_calibrated: source.acoustic.physically_calibrated,
                        occlusion_mode,
                        occlusion_radius_m,
                        occlusion_samples,
                        enabled: source.enabled,
                        muted: source.muted,
                        soloed: source.soloed,
                    }
                })
                .collect(),
            stages: self.stage_mix.into(),
            quality: self.capture_static.quality.clone(),
            listen_gain_db: self.monitor_gain_db,
            engine_config: self.capture_static.engine_config,
        }
    }

    fn capture_end_stats(&self) -> CaptureEndStats {
        match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(output) => {
                let telemetry = output.telemetry();
                CaptureEndStats {
                    callback_count: telemetry.callback_count,
                    window_p99_ms: telemetry.callback_timings.p99_ms,
                    window_p99_9_ms: telemetry.callback_timings.p99_9_ms,
                    run_p99_ms: telemetry.run_callback_timings.p99_ms,
                    run_p99_9_ms: telemetry.run_callback_timings.p99_9_ms,
                    deadline_misses: telemetry.deadline_misses,
                    processing_errors: telemetry.processing_errors,
                    stream_errors: telemetry.stream_errors,
                    snapshot_stale: telemetry.faults.snapshot_stale,
                    graph_deadline_miss: telemetry.faults.deadline_miss,
                    backend_render_error: telemetry.faults.backend_render_error,
                }
            }
            AudioState::Unavailable(_) => CaptureEndStats::default(),
        }
    }

    fn update_capture_lifecycle(&mut self) {
        if matches!(self.capture_state, CaptureUiState::Recording { .. })
            && !self.capture.is_requested()
        {
            self.capture_state = CaptureUiState::Stopping;
            if self.capture.was_auto_stopped() {
                self.capture_status = Some(format!(
                    "{} s limit reached; finalizing capture",
                    crate::capture::MAX_CAPTURE_SECONDS
                ));
            }
        }
        if matches!(self.capture_state, CaptureUiState::Stopping) && self.capture.ready_to_finish()
        {
            let stats = self.capture_end_stats();
            match self.capture.finish(stats) {
                Ok(()) => self.capture_state = CaptureUiState::Finishing,
                Err(error) => {
                    self.capture_state = CaptureUiState::Idle;
                    self.capture_status = Some(error);
                }
            }
        }
        if let Some(completion) = self.capture.poll_completion() {
            self.capture_state = CaptureUiState::Idle;
            self.capture_status = Some(match completion.result {
                Ok(()) => format!("Saved {}", completion.bundle.display()),
                Err(error) => format!("Capture failed: {error}"),
            });
            self.refresh_capture_browser();
        }
    }

    fn refresh_capture_browser(&mut self) {
        match scan_capture_bundles(self.capture.root()) {
            Ok(scan) => {
                self.capture_entries = scan.entries;
                self.capture_warnings = scan.warnings;
            }
            Err(error) => {
                self.capture_entries.clear();
                self.capture_warnings = vec![error];
            }
        }
    }

    fn save_mix_defaults(&mut self) {
        let defaults = MixDefaults {
            schema_version: MixDefaults::SCHEMA_VERSION,
            monitor_gain_db: self.monitor_gain_db,
            sources: self
                .sources
                .iter()
                .map(|source| SourceMixDefault {
                    id: source.id.clone(),
                    enabled: source.enabled,
                    muted: source.muted,
                    soloed: source.soloed,
                    monitor_offset_db: source.monitor_offset_db,
                    height: source.height.into(),
                })
                .collect(),
        };
        self.mix_defaults_status = Some(match defaults.write(&self.fixture_path) {
            Ok(path) => format!("Saved mix defaults to {}", path.display()),
            Err(error) => error,
        });
    }

    fn capture_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Capture");
        let audio_available = match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(_) => true,
            AudioState::Unavailable(_) => false,
        };
        let recording = matches!(self.capture_state, CaptureUiState::Recording { .. });
        let idle = matches!(self.capture_state, CaptureUiState::Idle);
        let button_label = if recording { "■ Stop" } else { "● Record" };
        if ui
            .add_enabled(
                audio_available && (idle || recording),
                egui::Button::new(button_label),
            )
            .clicked()
        {
            if recording {
                self.capture.request_stop();
                self.capture_state = CaptureUiState::Stopping;
                self.capture_status = Some("Draining capture blocks…".into());
            } else {
                let draft = self.capture_draft();
                match self.capture.start(draft) {
                    Ok(bundle) => {
                        self.capture_state = CaptureUiState::Recording { bundle };
                        self.capture_status = None;
                    }
                    Err(error) => self.capture_status = Some(error),
                }
            }
        }
        match &self.capture_state {
            CaptureUiState::Recording { bundle } => {
                ui.monospace(format!(
                    "REC {:6.1} / {} s",
                    self.capture.elapsed_seconds(),
                    crate::capture::MAX_CAPTURE_SECONDS
                ));
                ui.small(bundle.display().to_string());
            }
            CaptureUiState::Stopping => {
                ui.monospace("stopping · draining writer queue");
            }
            CaptureUiState::Finishing => {
                ui.monospace("writing manifest");
            }
            CaptureUiState::Idle => {}
        }
        if let Some(status) = &self.capture_status {
            ui.small(status);
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Capture browser");
            if ui.small_button("Refresh").clicked() {
                self.refresh_capture_browser();
            }
        });
        ui.small(self.capture.root().display().to_string());
        let mut reveal = None;
        for entry in &self.capture_entries {
            ui.group(|ui| {
                ui.monospace(&entry.timestamp);
                ui.small(format!(
                    "{:.1} s · {} · p99 {:.3} / p99.9 {:.3} ms · {} misses",
                    entry.duration_seconds,
                    entry.fixture_id,
                    entry.run_p99_ms,
                    entry.run_p99_9_ms,
                    entry.deadline_misses
                ));
                if ui.small_button("Reveal in Finder").clicked() {
                    reveal = Some(entry.bundle.join("capture.wav"));
                }
            });
        }
        if let Some(path) = reveal
            && let Err(error) = reveal_in_finder(&path)
        {
            self.capture_status = Some(error);
        }
        if let Some(warning) = self.capture_warnings.first() {
            ui.colored_label(
                Color32::from_rgb(255, 172, 90),
                format!("Skipped capture: {warning}"),
            );
        }
    }

    fn perf_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Fightbox");
        ui.label("WASD walk · Shift sprint");
        ui.label("Drag in view to turn head");
        ui.separator();
        ui.monospace(format!(
            "ENU  {:7.2}  {:7.2}  {:5.2} m",
            self.listener.position.east_m,
            self.listener.position.north_m,
            self.listener.position.up_m
        ));
        ui.monospace(format!(
            "yaw  {:6.1}°",
            self.listener.yaw_radians.to_degrees()
        ));
        ui.separator();
        ui.heading("Output safety");
        let mix_controls_enabled = matches!(self.capture_state, CaptureUiState::Idle);
        let monitor_gain_changed = ui
            .horizontal(|ui| {
                let changed = ui
                    .add_enabled(
                        mix_controls_enabled,
                        egui::Slider::new(
                            &mut self.monitor_gain_db,
                            MIN_MONITOR_GAIN_DB..=MAX_MONITOR_GAIN_DB,
                        )
                        .text("monitor gain"),
                    )
                    .changed();
                ui.monospace(format!("{:+.1} dB", self.monitor_gain_db));
                changed
            })
            .inner;
        if monitor_gain_changed {
            self.output_safety_controller
                .set_monitor_gain_db(self.monitor_gain_db)
                .expect("the monitor-gain slider publishes only finite values");
        }
        ui.small("Monitor gain is applied inside the guarded digital output chain.");
        ui.label("Free-field base prediction at listener");
        for source in &self.sources {
            let predicted_db = free_field_spl_at_listener_db(
                source.declared_spl_at_one_meter_db,
                source.position,
                self.listener.position,
                OutputSafetyConfig::DEFAULT_SOURCE_RADIUS_M,
            );
            ui.monospace(format!("{:<20} {:6.1} dB SPL", source.id, predicted_db));
        }
        ui.small(
            "Inverse-square from the fixture level only: excludes monitor offset, occlusion, \
             pathing and reflections. Not absolute SPL at the ear.",
        );
        let meter = self.meter_reader.read();
        ui.label("Digital output level · post-chain");
        ui.monospace(format!("peak  {:7.1} dBFS", meter.peak_dbfs));
        ui.monospace(format!("RMS   {:7.1} dBFS", meter.rms_dbfs));
        let safety = audio_safety_telemetry(&self.audio);
        engagement_row(
            ui,
            "proximity ceiling",
            safety
                .as_ref()
                .map(|telemetry| telemetry.proximity_ceiling_engagements),
        );
        engagement_row(
            ui,
            "true-peak limiter",
            safety
                .as_ref()
                .map(|telemetry| telemetry.limiter_engagements),
        );
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Sources");
            if ui
                .add_enabled(
                    mix_controls_enabled,
                    egui::Button::new("Save mix defaults").small(),
                )
                .clicked()
            {
                self.save_mix_defaults();
            }
        });
        if let Some(status) = &self.mix_defaults_status {
            ui.small(status);
        }
        let mut source_mix_changed = false;
        let mut source_height_changed = None;
        let source_height_levels = self.source_height_levels;
        for (index, source) in self.sources.iter_mut().enumerate() {
            ui.add_enabled_ui(mix_controls_enabled, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut source.enabled, "")
                        .on_hover_text("Enable this source")
                        .changed()
                    {
                        source_mix_changed = true;
                    }
                    if ui
                        .selectable_label(source.muted, "M")
                        .on_hover_text("Mute this source")
                        .clicked()
                    {
                        source.muted = !source.muted;
                        source_mix_changed = true;
                    }
                    if ui
                        .selectable_label(source.soloed, "S")
                        .on_hover_text("Solo this source")
                        .clicked()
                    {
                        source.soloed = !source.soloed;
                        source_mix_changed = true;
                    }
                    ui.monospace(&source.id);
                    if !source.enabled {
                        ui.small("disabled");
                    }
                    ui.monospace(format!(
                        "{} dB SPL",
                        format_db_number(source.declared_spl_at_one_meter_db)
                    ))
                    .on_hover_text("Fixture base SPL at 1 m");
                    ui.small("monitor offset");
                    if ui
                        .add(
                            egui::DragValue::new(&mut source.monitor_offset_db)
                                .range(MIN_SOURCE_OFFSET_DB..=MAX_SOURCE_OFFSET_DB)
                                .speed(0.1)
                                .suffix(" dB"),
                        )
                        .on_hover_text("Audition trim in the workbench playback layer; not physics")
                        .changed()
                    {
                        source.monitor_offset_db = clamp_source_offset_db(source.monitor_offset_db);
                        source_mix_changed = true;
                    }
                    ui.monospace(format_level_truth(
                        source.declared_spl_at_one_meter_db,
                        source.monitor_offset_db,
                    ));
                    if source.asset_id == ARTILLERY_ASSET_ID {
                        ui.small(format!("{ARTILLERY_RETRIGGER_SECONDS} s retrigger"));
                    }
                    // Every source is Steady with no protection left once the
                    // transient-event class is unused, so only surface the row
                    // when the backend actually reports protection.
                    if let Some((priority, remaining_blocks)) = source.acoustic.priority
                        && remaining_blocks > 0
                    {
                        ui.small(format!("{priority:?} · protect {remaining_blocks} blocks"));
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    ui.small("height");
                    for height in SourceHeight::ALL {
                        if ui
                            .selectable_label(source.height == height, height.label())
                            .clicked()
                        {
                            source.height = height;
                            source_height_changed = Some((index, height));
                        }
                    }
                    let resulting_height_m =
                        source_height_levels.height_m(source.height, source.street_height_m);
                    ui.monospace(format!("z {resulting_height_m:.1} m"));
                });
            });
            acoustic_badge_row(ui, source.acoustic, source.occlusion_mode);
        }
        if let Some((index, height)) = source_height_changed {
            self.apply_source_height(index, height);
        }
        if source_mix_changed {
            self.source_mix_writer
                .publish(SourceMix::from_sources(&self.sources));
        }
        ui.separator();
        ui.heading("Stages");
        let mut stage_mix_changed = false;
        for (index, label) in ["Direct", "Pathing", "Reflections"].into_iter().enumerate() {
            ui.add_enabled_ui(mix_controls_enabled, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(self.stage_mix.bypassed[index], "B")
                        .on_hover_text("Bypass this stage")
                        .clicked()
                    {
                        self.stage_mix.bypassed[index] = !self.stage_mix.bypassed[index];
                        stage_mix_changed = true;
                    }
                    if ui
                        .selectable_label(self.stage_mix.soloed[index], "S")
                        .on_hover_text("Solo this stage")
                        .clicked()
                    {
                        self.stage_mix.soloed[index] = !self.stage_mix.soloed[index];
                        stage_mix_changed = true;
                    }
                    ui.monospace(label);
                });
            });
        }
        if stage_mix_changed {
            self.stage_output_gain_control
                .publish(self.stage_mix.gains())
                .expect("stage toggle gains are finite and non-negative");
        }
        ui.separator();
        ui.heading("Autopilot");
        let was_enabled = self.autopilot.enabled;
        ui.checkbox(&mut self.autopilot.enabled, "follow city circuit");
        if self.autopilot.enabled && !was_enabled {
            self.autopilot.reset();
        }
        ui.add(
            egui::Slider::new(&mut self.autopilot.speed_mps, 1.0..=30.0)
                .suffix(" m/s")
                .text("speed"),
        );
        ui.separator();
        ui.heading("Audio callback");
        match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(output) => {
                let telemetry = output.telemetry();
                ui.monospace(format!(
                    "window p99    {:6.3} ms",
                    telemetry.callback_timings.p99_ms
                ));
                ui.monospace(format!(
                    "window p99.9  {:6.3} ms",
                    telemetry.callback_timings.p99_9_ms
                ));
                ui.monospace(format!(
                    "run p99       {:6.3} ms",
                    telemetry.run_callback_timings.p99_ms
                ));
                ui.monospace(format!(
                    "run p99.9     {:6.3} ms",
                    telemetry.run_callback_timings.p99_9_ms
                ));
                ui.separator();
                ui.monospace(format!("callbacks      {}", telemetry.callback_count));
                ui.monospace(format!("deadline miss  {}", telemetry.deadline_misses));
                ui.monospace(format!("process error  {}", telemetry.processing_errors));
                ui.monospace(format!("stream error   {}", telemetry.stream_errors));
                fault_rows(ui, telemetry.faults);
            }
            AudioState::Unavailable(message) => {
                ui.colored_label(Color32::from_rgb(255, 172, 90), "Audio unavailable");
                ui.label(message);
            }
        }
        let simulation = self.simulation.telemetry();
        ui.separator();
        ui.heading("Simulation");
        ui.monospace(format!(
            "failures d/p/r  {}/{}/{}",
            simulation.direct.failures,
            simulation.pathing.failures,
            simulation.reflections.failures
        ));
        let acoustic = self.acoustic_telemetry.read();
        match acoustic.governor {
            Some(governor) => {
                ui.monospace(format!(
                    "governor rung {}  {:?}  last {:?}",
                    governor.ladder_position, governor.reflection_level, governor.reason
                ));
                #[cfg(fightbox_governor_boot_telemetry)]
                ui.monospace(format!(
                    "boot decision {:?}  predicted {:.3} ms / {:.3} ms admission  p99 budget {:.3} ms",
                    governor.boot_reflection_level,
                    governor.boot_predicted_cost_ns as f64 / 1_000_000.0,
                    governor.boot_cost_limit_ns as f64 / 1_000_000.0,
                    governor.boot_p99_budget_ns as f64 / 1_000_000.0,
                ));
                ui.monospace(format!(
                    "first delivered rung {}  refl {:?}  path {:?}  M{}",
                    governor.observed_boot_ladder_position,
                    governor.observed_boot_reflection_level,
                    governor.observed_boot_pathing,
                    governor.observed_boot_ambisonic_order,
                ));
                ui.monospace(format!(
                    "reflections {} rays / {} bounces / {:.2} s / cadence ÷{}  gain {:.3}",
                    governor.reflection_rays,
                    governor.reflection_bounces,
                    governor.reflection_ir_duration_s,
                    governor.reflection_cadence_divisor,
                    governor.reflection_output_gain,
                ));
                ui.monospace(format!(
                    "path {:?}  ambisonic M{}",
                    governor.pathing, governor.ambisonic_order
                ));
            }
            None if acoustic.known => {
                ui.monospace("governor ungoverned for this generation");
            }
            None => {
                ui.monospace("governor telemetry awaiting first direct pass");
            }
        }
        let visibility_text = format!(
            "visibility {:.2} m configured -> {:.2} m effective  spacing {:.2} m{}",
            self.visibility_range.configured_m,
            self.visibility_range.effective_m,
            self.visibility_range.probe_spacing_m,
            if self.visibility_range.rebaselined {
                "  RE-BASELINED"
            } else {
                ""
            },
        );
        if self.visibility_range.rebaselined {
            ui.colored_label(Color32::from_rgb(255, 172, 90), visibility_text);
        } else {
            ui.monospace(visibility_text);
        }
        ui.separator();
        self.capture_panel(ui);
    }
}

impl Workbench {
    fn update_ui(&mut self, ctx: &egui::Context) {
        self.update_source_motion();
        self.refresh_acoustic_state();
        self.update_capture_lifecycle();
        egui::SidePanel::right("performance")
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.perf_panel(ui));
            });
        let mut drag_delta_x = 0.0;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let (response, painter) =
                    ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
                if response.dragged_by(egui::PointerButton::Primary) {
                    drag_delta_x = ui.input(|input| input.pointer.delta().x);
                }
                self.draw_first_person(&painter, response.rect);
                let pip_rect = picture_in_picture_rect(response.rect);
                self.draw_scene(&painter, pip_rect);
                painter.rect_stroke(
                    pip_rect,
                    3.0,
                    Stroke::new(1.0, Color32::from_rgb(105, 136, 153)),
                    egui::StrokeKind::Inside,
                );
                painter.text(
                    pip_rect.left_top() + egui::vec2(8.0, 7.0),
                    egui::Align2::LEFT_TOP,
                    "MAP VIEW",
                    egui::FontId::monospace(10.0),
                    Color32::from_rgb(142, 173, 188),
                );
            });
        self.update_control(ctx, drag_delta_x);
        ctx.request_repaint();
        if !self.reflection_warmup_reported {
            let telemetry = self.simulation.telemetry();
            if let Some(pass_ns) = telemetry.reflections.timings.newest_ns() {
                eprintln!(
                    "[startup] reflection warmup: {} ms (pass {} ms)",
                    self.reflection_warmup_started.elapsed().as_millis(),
                    pass_ns / 1_000_000
                );
                self.reflection_warmup_reported = true;
            }
        }
        if !self.first_frame_reported {
            eprintln!(
                "[startup] total to first frame: {} ms",
                self.startup_started.elapsed().as_millis()
            );
            self.first_frame_reported = true;
        }
    }
}

impl eframe::App for WorkbenchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut requested_scene = None;
        if self.scenes.len() > 1 {
            if ctx.input(|input| input.key_pressed(egui::Key::T)) {
                requested_scene = Some((self.active_scene_index + 1) % self.scenes.len());
            }
            egui::TopBottomPanel::top("scene-tabs").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!(
                        "Scene: {}",
                        self.scenes[self.active_scene_index].id
                    ));
                    ui.separator();
                    for (index, scene) in self.scenes.iter().enumerate() {
                        if ui
                            .selectable_label(index == self.active_scene_index, &scene.id)
                            .clicked()
                        {
                            requested_scene = Some(index);
                        }
                    }
                    ui.separator();
                    ui.small("T cycles scenes");
                });
                if let Some(status) = &self.scene_status {
                    ui.small(status);
                }
            });
        }
        if let Some(scene_index) = requested_scene
            && scene_index != self.active_scene_index
        {
            self.rebuild(scene_index, "scene switch");
        }

        if let Some(active) = &mut self.active {
            active.update_ui(ctx);
        } else {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Scene unavailable");
                if let Some(status) = &self.scene_status {
                    ui.label(status);
                }
            });
        }
    }
}

fn audio_safety_telemetry(audio: &AudioState) -> Option<SafetyTelemetry> {
    match audio {
        #[cfg(feature = "live-output")]
        AudioState::Live(output) => Some(output.telemetry().safety),
        AudioState::Unavailable(_) => None,
    }
}

const ACOUSTIC_BADGE_HOVER: &str = concat!(
    "Baked path-probe coverage at this source's current position and at the listener. ",
    "A source outside every probe has no baked path and falls back to direct-only. ",
    "occl is Steam Audio direct occlusion audibility, 1.00 clear to 0.00 fully occluded.\n",
    "Stage chips: + contributing, - silent, ? not reported by the session.\n",
    "Volumetric parameters are effective per source: point and multi-point default to a 1 m ",
    "radius; line and stereo extents use half their declared width; n is the fixture sample count.\n",
    "Quality and calibration come from the governor. Path SH energy and EQ come from the latest ",
    "per-source simulation snapshot.",
);

/// One compact row per source: probe coverage, direct occlusion, and the render
/// stages that can currently contribute.
fn acoustic_badge_row(
    ui: &mut egui::Ui,
    state: SourceAcousticState,
    occlusion_mode: DirectOcclusionMode,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(22.0);
        ui.colored_label(
            badge_color(probe_tone(state)),
            badge_text(probe_text(state)),
        );
        for (text, tone) in stage_chips(state) {
            ui.colored_label(badge_color(tone), badge_text(text));
        }
    })
    .response
    .on_hover_text(ACOUSTIC_BADGE_HOVER);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(22.0);
        ui.colored_label(
            badge_color(quality_tone(state)),
            badge_text(quality_text(state)),
        );
        ui.colored_label(
            badge_color(BadgeTone::Ok),
            badge_text(occlusion_mode_text(occlusion_mode)),
        );
    });
    ui.horizontal(|ui| {
        ui.add_space(22.0);
        ui.colored_label(
            badge_color(if state.path_sh_energy.is_some() {
                BadgeTone::Ok
            } else {
                BadgeTone::Unknown
            }),
            badge_text(path_diagnostics_text(state)),
        );
    });
}

fn occlusion_mode_text(mode: DirectOcclusionMode) -> String {
    match mode {
        DirectOcclusionMode::Raycast => "occlusion raycast".to_owned(),
        DirectOcclusionMode::Volumetric {
            radius_m,
            sample_count,
        } => format!("occlusion volumetric r={radius_m:.2} m n={sample_count}"),
    }
}

fn badge_text(text: String) -> egui::RichText {
    egui::RichText::new(text)
        .family(egui::FontFamily::Monospace)
        .small()
}

fn badge_color(tone: BadgeTone) -> Color32 {
    match tone {
        BadgeTone::Ok => Color32::from_rgb(112, 180, 155),
        BadgeTone::Warn => Color32::from_rgb(255, 172, 90),
        BadgeTone::Off => Color32::from_rgb(101, 114, 124),
        BadgeTone::Unknown => Color32::from_rgb(142, 173, 188),
    }
}

fn engagement_row(ui: &mut egui::Ui, label: &str, engagements: Option<u64>) {
    let (color, state) = match engagements {
        None => (
            Color32::from_rgb(142, 173, 188),
            "telemetry unavailable".to_owned(),
        ),
        Some(0) => (Color32::from_rgb(112, 180, 155), "idle".to_owned()),
        Some(count) => (
            Color32::from_rgb(255, 172, 90),
            format!("engaged this run · {count}"),
        ),
    };
    ui.colored_label(
        color,
        egui::RichText::new(format!("{label:<20} {state}")).family(egui::FontFamily::Monospace),
    );
}

/// Free-field inverse-square falloff from the fixture's declared level alone.
///
/// This is deliberately not a render prediction: it sees no monitor offset, no
/// occlusion or transmission, and nothing from pathing or reflections.
fn free_field_spl_at_listener_db(
    declared_spl_at_one_meter_db: f32,
    source_position: EnuVector3,
    listener_position: EnuVector3,
    source_radius_m: f32,
) -> f32 {
    let distance_m = vector_length(subtract(source_position, listener_position));
    declared_spl_at_one_meter_db - 20.0 * distance_m.max(source_radius_m).log10()
}

#[cfg(feature = "live-output")]
fn fault_rows(ui: &mut egui::Ui, faults: fightbox_runtime::FaultCounters) {
    ui.monospace(format!("snapshot stale {}", faults.snapshot_stale));
    ui.monospace(format!("graph deadline {}", faults.deadline_miss));
    ui.monospace(format!("backend error  {}", faults.backend_render_error));
}

fn axis(input: &egui::InputState, positive: egui::Key, negative: egui::Key) -> f32 {
    f32::from(input.key_down(positive)) - f32::from(input.key_down(negative))
}

trait ListenerStateSink {
    fn set_listener_state(&mut self, listener: ListenerState);
}

impl ListenerStateSink for RuntimeGraph {
    fn set_listener_state(&mut self, listener: ListenerState) {
        RuntimeGraph::set_listener_state(self, listener);
    }
}

struct LateBoundProcessor<P> {
    processor: P,
    pose_reader: SnapshotReader<ListenerState>,
    meter_writer: SnapshotWriter<MeterReading>,
    meter: MeterAccumulator,
    audio_block_writer: SnapshotWriter<u64>,
    capture_tap: Option<CaptureTap>,
    elapsed_blocks: u64,
}

impl<P> LateBoundProcessor<P> {
    fn new(
        processor: P,
        pose_reader: SnapshotReader<ListenerState>,
        meter_writer: SnapshotWriter<MeterReading>,
        meter: MeterAccumulator,
        audio_block_writer: SnapshotWriter<u64>,
        capture_tap: Option<CaptureTap>,
    ) -> Self {
        Self {
            processor,
            pose_reader,
            meter_writer,
            meter,
            audio_block_writer,
            capture_tap,
            elapsed_blocks: 0,
        }
    }
}

impl<P: BlockProcessor + ListenerStateSink> BlockProcessor for LateBoundProcessor<P> {
    fn block_size_frames(&self) -> usize {
        self.processor.block_size_frames()
    }

    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
        let listener = self.pose_reader.read();
        self.processor.set_listener_state(listener);
        let ProcessBlock {
            now_ns,
            sources,
            output_left,
            output_right,
        } = block;
        self.processor.process_block(ProcessBlock {
            now_ns,
            sources,
            output_left: &mut *output_left,
            output_right: &mut *output_right,
        })?;
        let reading = self.meter.observe(output_left, output_right);
        self.meter_writer.publish(reading);
        if let Some(capture_tap) = &self.capture_tap {
            capture_tap.capture_block(output_left, output_right);
        }
        self.elapsed_blocks = self.elapsed_blocks.saturating_add(1);
        self.audio_block_writer.publish(self.elapsed_blocks);
        Ok(())
    }

    fn fault_counters(&self) -> fightbox_runtime::FaultCounters {
        self.processor.fault_counters()
    }

    fn safety_telemetry(&self) -> SafetyTelemetry {
        self.processor.safety_telemetry()
    }
}

fn capture_quality(
    config: S3SimulationConfig,
    visibility: VisibilityRangeAdoption,
) -> CaptureQualitySettings {
    let cadences = SimulationCadences::default();
    CaptureQualitySettings {
        direct_occlusion: match config.direct_occlusion {
            DirectOcclusionMode::Raycast => "raycast".into(),
            DirectOcclusionMode::Volumetric { .. } => "volumetric".into(),
        },
        direct_occlusion_radius_m: match config.direct_occlusion {
            DirectOcclusionMode::Raycast => None,
            DirectOcclusionMode::Volumetric { radius_m, .. } => Some(radius_m),
        },
        direct_occlusion_samples: match config.direct_occlusion {
            DirectOcclusionMode::Raycast => None,
            DirectOcclusionMode::Volumetric { sample_count, .. } => Some(sample_count),
        },
        max_occlusion_samples: config.max_occlusion_samples,
        reflection_effect: format!("{:?}", config.reflection_effect.effect_type).to_lowercase(),
        reflection_rays: config.reflection_rays,
        reflection_bounces: config.reflection_bounces,
        reflection_duration_s: config.reflection_duration_s,
        reflection_order: config.reflection_order,
        pathing_order: config.pathing_order,
        pathing_visibility_range_configured_m: visibility.configured_m,
        probe_spacing_m: visibility.probe_spacing_m,
        pathing_visibility_range_m: visibility.effective_m,
        pathing_visibility_range_rebaselined: visibility.rebaselined,
        validate_paths: config.validate_paths,
        find_alternate_paths: config.find_alternate_paths,
        direct_simulation_hz: cadences.direct_hz,
        pathing_simulation_hz: cadences.pathing_hz,
        reflections_simulation_hz: cadences.reflections_hz,
        reflection_max_displacement_m: cadences.reflection_max_displacement_m,
        reflection_max_hz: cadences.reflection_max_hz,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MeterReading {
    peak_dbfs: f32,
    rms_dbfs: f32,
}

impl MeterReading {
    const SILENT: Self = Self {
        peak_dbfs: -120.0,
        rms_dbfs: -120.0,
    };
}

#[derive(Clone, Copy, Debug, Default)]
struct MeterBlock {
    peak: f32,
    square_sum: f64,
    samples: usize,
}

struct MeterAccumulator {
    blocks: Vec<MeterBlock>,
    next: usize,
    square_sum: f64,
    samples: usize,
}

impl MeterAccumulator {
    fn new(sample_rate: u32, block_size: u32, window_seconds: f32) -> Self {
        let block_count =
            ((sample_rate as f32 * window_seconds) / block_size as f32).ceil() as usize;
        Self {
            blocks: vec![MeterBlock::default(); block_count.max(1)],
            next: 0,
            square_sum: 0.0,
            samples: 0,
        }
    }

    fn observe(&mut self, left: &[f32], right: &[f32]) -> MeterReading {
        let outgoing = self.blocks[self.next];
        self.square_sum -= outgoing.square_sum;
        self.samples -= outgoing.samples;
        let mut incoming = MeterBlock::default();
        for sample in left.iter().chain(right) {
            incoming.peak = incoming.peak.max(sample.abs());
            incoming.square_sum += f64::from(*sample) * f64::from(*sample);
            incoming.samples += 1;
        }
        self.blocks[self.next] = incoming;
        self.next = (self.next + 1) % self.blocks.len();
        self.square_sum += incoming.square_sum;
        self.samples += incoming.samples;
        let peak = self
            .blocks
            .iter()
            .map(|block| block.peak)
            .fold(0.0_f32, f32::max);
        let rms = if self.samples == 0 {
            0.0
        } else {
            (self.square_sum / self.samples as f64).sqrt() as f32
        };
        MeterReading {
            peak_dbfs: amplitude_dbfs(peak),
            rms_dbfs: amplitude_dbfs(rms),
        }
    }
}

fn amplitude_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        -120.0
    } else {
        (20.0 * amplitude.log10()).max(-120.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackMode {
    Looping,
    PeriodicOneShot { interval_frames: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourcePlayback {
    mode: PlaybackMode,
    cursor: usize,
    was_enabled: bool,
}

impl SourcePlayback {
    fn for_asset(asset_id: &str, sample_rate: u32) -> Self {
        let mode = if asset_id == ARTILLERY_ASSET_ID {
            PlaybackMode::PeriodicOneShot {
                interval_frames: artillery_retrigger_frames(sample_rate),
            }
        } else {
            PlaybackMode::Looping
        };
        Self {
            mode,
            cursor: 0,
            was_enabled: false,
        }
    }

    fn next_sample(&mut self, signal: &[f32], enabled: bool) -> f32 {
        debug_assert!(!signal.is_empty());
        match self.mode {
            PlaybackMode::Looping => {
                let sample = signal[self.cursor];
                self.cursor = (self.cursor + 1) % signal.len();
                sample
            }
            PlaybackMode::PeriodicOneShot { interval_frames } => {
                if !enabled {
                    self.cursor = 0;
                    self.was_enabled = false;
                    return 0.0;
                }
                if !self.was_enabled {
                    self.cursor = 0;
                    self.was_enabled = true;
                }
                let sample = signal.get(self.cursor).copied().unwrap_or(0.0);
                self.cursor = (self.cursor + 1) % interval_frames;
                sample
            }
        }
    }
}

fn artillery_retrigger_frames(sample_rate: u32) -> usize {
    sample_rate as usize * ARTILLERY_RETRIGGER_SECONDS as usize
}

#[cfg(feature = "live-output")]
struct WorkbenchInput {
    signals: Vec<Vec<f32>>,
    playback: Vec<SourcePlayback>,
    source_mix_reader: SnapshotReader<SourceMix>,
}

#[cfg(feature = "live-output")]
impl fightbox_runtime::live::LiveInputProvider for WorkbenchInput {
    fn fill_block(&mut self, sources: &mut fightbox_runtime::live::LiveSourceBuffer) {
        let mix = self.source_mix_reader.read();
        let gains = mix.gains(self.signals.len());
        for index in 0..self.signals.len() {
            let Some(output) = sources.add_source(index) else {
                return;
            };
            let signal = &self.signals[index];
            for sample in output {
                *sample =
                    self.playback[index].next_sample(signal, mix.enabled[index]) * gains[index];
            }
        }
    }
}

#[cfg(feature = "live-output")]
fn start_audio<P: BlockProcessor + Send + 'static>(
    processor: P,
    config: EngineConfig,
    signals: Vec<Vec<f32>>,
    playback: Vec<SourcePlayback>,
    source_mix_reader: SnapshotReader<SourceMix>,
    device: Option<&str>,
) -> AudioState {
    let input = Box::new(WorkbenchInput {
        signals,
        playback,
        source_mix_reader,
    });
    let output = match device {
        Some(name) => {
            fightbox_runtime::live::LiveOutput::new_named_with_input(processor, config, name, input)
        }
        None => {
            fightbox_runtime::live::LiveOutput::new_default_with_input(processor, config, input)
        }
    };
    match output {
        Ok(output) => match output.start() {
            Ok(()) => AudioState::Live(output),
            Err(error) => AudioState::Unavailable(format!("cannot start output: {error:?}")),
        },
        Err(error) => AudioState::Unavailable(format!("cannot open output: {error:?}")),
    }
}

#[cfg(not(feature = "live-output"))]
fn start_audio<P: BlockProcessor + Send + 'static>(
    _processor: P,
    _config: EngineConfig,
    _signals: Vec<Vec<f32>>,
    _playback: Vec<SourcePlayback>,
    _source_mix_reader: SnapshotReader<SourceMix>,
    _device: Option<&str>,
) -> AudioState {
    AudioState::Unavailable("binary was built without the live-output feature".into())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MeshFace {
    indices: [usize; 3],
    normal: [f32; 3],
    is_ground: bool,
}

fn mesh_faces(mesh: &AcousticMesh) -> Vec<MeshFace> {
    let ground_height = mesh
        .vertices_enu_m
        .iter()
        .map(|vertex| vertex.up_m)
        .reduce(f32::min)
        .unwrap_or_default();
    mesh.triangles
        .iter()
        .map(|triangle| {
            let indices = triangle.map(|index| index as usize);
            let [a, b, c] = indices.map(|index| mesh.vertices_enu_m[index]);
            let normal = normalize3(cross3(point3(b, a), point3(c, a)));
            let is_ground = normal[2].abs() >= 0.95
                && [a, b, c]
                    .iter()
                    .all(|vertex| (vertex.up_m - ground_height).abs() <= 1.0e-3);
            MeshFace {
                indices,
                normal,
                is_ground,
            }
        })
        .collect()
}

struct ProjectedFace {
    points: [Pos2; 4],
    point_count: usize,
    depth: f32,
    fill: Color32,
}

fn project_face(
    mesh: &AcousticMesh,
    face: MeshFace,
    rect: Rect,
    camera_point: impl Fn(EnuVector3) -> [f32; 3],
    screen_point: impl Fn([f32; 3], Rect) -> Pos2,
) -> Option<ProjectedFace> {
    let camera_points = face
        .indices
        .map(|index| camera_point(mesh.vertices_enu_m[index]));
    let clipped = clip_polygon_to_near(&camera_points, FIRST_PERSON_NEAR_M);
    if clipped.point_count < 3 {
        return None;
    }
    let depth = polygon_depth(&clipped.points[..clipped.point_count]);
    let mut points = [Pos2::ZERO; 4];
    for (destination, point) in points
        .iter_mut()
        .zip(&clipped.points[..clipped.point_count])
    {
        *destination = screen_point(*point, rect);
    }
    Some(ProjectedFace {
        points,
        point_count: clipped.point_count,
        depth,
        fill: face_color(face),
    })
}

fn paint_faces(painter: &egui::Painter, faces: &mut [ProjectedFace]) {
    painter.add(egui::Shape::mesh(projected_faces_mesh(faces)));
}

fn projected_faces_mesh(faces: &mut [ProjectedFace]) -> egui::Mesh {
    faces.sort_by(|left, right| right.depth.total_cmp(&left.depth));
    let mut mesh = egui::Mesh::default();
    mesh.vertices.reserve(faces.len() * 4);
    mesh.indices.reserve(faces.len() * 6);
    for face in faces {
        let first = mesh.vertices.len() as u32;
        for &point in &face.points[..face.point_count] {
            mesh.colored_vertex(point, face.fill);
        }
        for index in 1..face.point_count - 1 {
            mesh.add_triangle(first, first + index as u32, first + index as u32 + 1);
        }
    }
    mesh
}

fn face_color(face: MeshFace) -> Color32 {
    let brightness = face_brightness(face.normal);
    let base = if face.is_ground {
        [55, 67, 62]
    } else {
        [104, 132, 145]
    };
    Color32::from_rgb(
        (base[0] as f32 * brightness).round() as u8,
        (base[1] as f32 * brightness).round() as u8,
        (base[2] as f32 * brightness).round() as u8,
    )
}

fn face_brightness(normal: [f32; 3]) -> f32 {
    const LIGHT_DIRECTION: [f32; 3] = [-0.44, -0.57, 0.69];
    (0.46 + 0.54 * dot3(normal, LIGHT_DIRECTION).abs()).clamp(0.46, 1.0)
}

fn polygon_depth(points: &[[f32; 3]]) -> f32 {
    points.iter().map(|point| point[2]).sum::<f32>() / points.len() as f32
}

struct ClippedPolygon {
    points: [[f32; 3]; 4],
    point_count: usize,
}

fn clip_polygon_to_near(points: &[[f32; 3]], near_m: f32) -> ClippedPolygon {
    let mut clipped = ClippedPolygon {
        points: [[0.0; 3]; 4],
        point_count: 0,
    };
    let mut previous = *points.last().expect("a face has three vertices");
    let mut previous_inside = previous[2] >= near_m;
    for &current in points {
        let current_inside = current[2] >= near_m;
        if current_inside != previous_inside {
            clipped.points[clipped.point_count] = clip_to_depth(previous, current, near_m);
            clipped.point_count += 1;
        }
        if current_inside {
            clipped.points[clipped.point_count] = current;
            clipped.point_count += 1;
        }
        previous = current;
        previous_inside = current_inside;
    }
    clipped
}

fn picture_in_picture_rect(container: Rect) -> Rect {
    let available_width = (container.width() - PICTURE_IN_PICTURE_MARGIN * 2.0).max(1.0);
    let available_height = (container.height() - PICTURE_IN_PICTURE_MARGIN * 2.0).max(1.0);
    let size = egui::vec2(
        (container.width() * 0.32).max(260.0).min(available_width),
        (container.height() * 0.30).max(170.0).min(available_height),
    );
    Rect::from_min_size(
        Pos2::new(
            container.right() - PICTURE_IN_PICTURE_MARGIN - size.x,
            container.top() + PICTURE_IN_PICTURE_MARGIN,
        ),
        size,
    )
}

#[derive(Clone, Copy)]
struct Camera {
    eye: [f32; 3],
    target: [f32; 3],
}

impl Camera {
    fn for_mesh(mesh: &AcousticMesh) -> Self {
        let first = mesh.vertices_enu_m.first().copied().unwrap_or_default();
        let mut min = [first.east_m, first.north_m, first.up_m];
        let mut max = min;
        for vertex in &mesh.vertices_enu_m {
            let point = [vertex.east_m, vertex.north_m, vertex.up_m];
            for axis in 0..3 {
                min[axis] = min[axis].min(point[axis]);
                max[axis] = max[axis].max(point[axis]);
            }
        }
        let target = [
            (min[0] + max[0]) * 0.5,
            (min[1] + max[1]) * 0.5,
            (min[2] + max[2]) * 0.5,
        ];
        let radius = (max[0] - min[0])
            .max(max[1] - min[1])
            .max(max[2] - min[2])
            .max(10.0);
        Self {
            eye: [
                target[0] + radius * 0.85,
                target[1] - radius * 0.95,
                target[2] + radius * 0.75,
            ],
            target,
        }
    }

    fn project(self, point: EnuVector3, rect: Rect) -> Option<Pos2> {
        let camera = self.camera_point(point);
        (camera[2] > FIRST_PERSON_NEAR_M).then(|| self.screen_point(camera, rect))
    }

    fn camera_point(self, point: EnuVector3) -> [f32; 3] {
        let forward = normalize3(sub3(self.target, self.eye));
        let right = normalize3(cross3(forward, [0.0, 0.0, 1.0]));
        let up = cross3(right, forward);
        let relative = sub3([point.east_m, point.north_m, point.up_m], self.eye);
        [
            dot3(relative, right),
            dot3(relative, up),
            dot3(relative, forward),
        ]
    }

    fn screen_point(self, point: [f32; 3], rect: Rect) -> Pos2 {
        let scale = rect.height().min(rect.width()) * 0.9 / point[2];
        Pos2::new(
            rect.center().x + point[0] * scale,
            rect.center().y - point[1] * scale,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds2 {
    min: [f32; 2],
    max: [f32; 2],
}

impl Bounds2 {
    fn for_mesh(mesh: &AcousticMesh) -> Self {
        let first = mesh.vertices_enu_m.first().copied().unwrap_or_default();
        let mut bounds = Self {
            min: [first.east_m, first.north_m],
            max: [first.east_m, first.north_m],
        };
        for vertex in &mesh.vertices_enu_m {
            bounds.min[0] = bounds.min[0].min(vertex.east_m);
            bounds.min[1] = bounds.min[1].min(vertex.north_m);
            bounds.max[0] = bounds.max[0].max(vertex.east_m);
            bounds.max[1] = bounds.max[1].max(vertex.north_m);
        }
        bounds
    }

    fn inset_circuit(self) -> RectCircuit {
        let width = self.max[0] - self.min[0];
        let height = self.max[1] - self.min[1];
        // A proportional inset lands on the first interior street of regular
        // city grids while still producing a useful circuit for small scenes.
        let inset = width.min(height) * 0.16;
        RectCircuit {
            min: [self.min[0] + inset, self.min[1] + inset],
            max: [self.max[0] - inset, self.max[1] - inset],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RectCircuit {
    min: [f32; 2],
    max: [f32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CircuitSample {
    position: [f32; 2],
    direction: [f32; 2],
}

impl RectCircuit {
    fn perimeter(self) -> f32 {
        2.0 * ((self.max[0] - self.min[0]) + (self.max[1] - self.min[1]))
    }

    fn sample(self, distance: f32) -> CircuitSample {
        let width = self.max[0] - self.min[0];
        let height = self.max[1] - self.min[1];
        let mut distance = distance.rem_euclid(self.perimeter());
        if distance < width {
            return CircuitSample {
                position: [self.min[0] + distance, self.min[1]],
                direction: [1.0, 0.0],
            };
        }
        distance -= width;
        if distance < height {
            return CircuitSample {
                position: [self.max[0], self.min[1] + distance],
                direction: [0.0, 1.0],
            };
        }
        distance -= height;
        if distance < width {
            return CircuitSample {
                position: [self.max[0] - distance, self.max[1]],
                direction: [-1.0, 0.0],
            };
        }
        distance -= width;
        CircuitSample {
            position: [self.min[0], self.max[1] - distance],
            direction: [0.0, -1.0],
        }
    }

    fn distance_for_position(self, position: EnuVector3) -> f32 {
        let width = self.max[0] - self.min[0];
        let height = self.max[1] - self.min[1];
        let candidates = [
            (
                [position.east_m.clamp(self.min[0], self.max[0]), self.min[1]],
                position.east_m.clamp(self.min[0], self.max[0]) - self.min[0],
            ),
            (
                [
                    self.max[0],
                    position.north_m.clamp(self.min[1], self.max[1]),
                ],
                width + position.north_m.clamp(self.min[1], self.max[1]) - self.min[1],
            ),
            (
                [position.east_m.clamp(self.min[0], self.max[0]), self.max[1]],
                width + height + self.max[0] - position.east_m.clamp(self.min[0], self.max[0]),
            ),
            (
                [
                    self.min[0],
                    position.north_m.clamp(self.min[1], self.max[1]),
                ],
                2.0 * width + height + self.max[1]
                    - position.north_m.clamp(self.min[1], self.max[1]),
            ),
        ];
        candidates
            .into_iter()
            .min_by(|left, right| {
                let left_distance =
                    (left.0[0] - position.east_m).powi(2) + (left.0[1] - position.north_m).powi(2);
                let right_distance = (right.0[0] - position.east_m).powi(2)
                    + (right.0[1] - position.north_m).powi(2);
                left_distance.total_cmp(&right_distance)
            })
            .map_or(0.0, |candidate| candidate.1)
            .rem_euclid(self.perimeter())
    }
}

struct Autopilot {
    enabled: bool,
    speed_mps: f32,
    distance_m: f32,
    circuit: RectCircuit,
}

impl Autopilot {
    fn new(bounds: Bounds2) -> Self {
        Self {
            enabled: false,
            speed_mps: DEFAULT_AUTOPILOT_SPEED_MPS,
            distance_m: 0.0,
            circuit: bounds.inset_circuit(),
        }
    }

    fn for_scene(
        bounds: Bounds2,
        fixture: &Fixture,
        scene_id: &str,
        listener_position: EnuVector3,
    ) -> Self {
        let mut autopilot = Self::new(bounds);
        if scene_id != "checkpoint-block" {
            return autopilot;
        }
        let Some(trajectory) = &fixture.listener.trajectory else {
            return autopilot;
        };
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        for waypoint in &trajectory.waypoints_m {
            min[0] = min[0].min(waypoint[0] as f32);
            min[1] = min[1].min(waypoint[1] as f32);
            max[0] = max[0].max(waypoint[0] as f32);
            max[1] = max[1].max(waypoint[1] as f32);
        }
        if trajectory.waypoints_m.len() == 4 && min[0] < max[0] && min[1] < max[1] {
            autopilot.circuit = RectCircuit { min, max };
            autopilot.speed_mps = trajectory.speed_mps as f32;
            autopilot.distance_m = autopilot.circuit.distance_for_position(listener_position);
            autopilot.enabled = true;
        }
        autopilot
    }

    fn reset(&mut self) {
        self.distance_m = 0.0;
    }

    fn advance(&mut self, delta_seconds: f32) -> CircuitSample {
        self.distance_m =
            (self.distance_m + self.speed_mps * delta_seconds).rem_euclid(self.circuit.perimeter());
        self.circuit.sample(self.distance_m)
    }
}

#[derive(Clone, Copy)]
struct FirstPersonProjection {
    eye: EnuVector3,
    forward: [f32; 2],
    right: [f32; 2],
    tan_half_vertical_fov: f32,
    near_m: f32,
}

impl FirstPersonProjection {
    fn new(eye: EnuVector3, yaw_radians: f32, vertical_fov_radians: f32, near_m: f32) -> Self {
        Self {
            eye,
            forward: [yaw_radians.sin(), yaw_radians.cos()],
            right: [yaw_radians.cos(), -yaw_radians.sin()],
            tan_half_vertical_fov: (vertical_fov_radians * 0.5).tan(),
            near_m,
        }
    }

    fn camera_point(self, point: EnuVector3) -> [f32; 3] {
        let east = point.east_m - self.eye.east_m;
        let north = point.north_m - self.eye.north_m;
        [
            east * self.right[0] + north * self.right[1],
            point.up_m - self.eye.up_m,
            east * self.forward[0] + north * self.forward[1],
        ]
    }

    fn screen_point(self, point: [f32; 3], rect: Rect) -> Pos2 {
        let aspect = rect.width() / rect.height().max(1.0);
        let x = point[0] / (point[2] * self.tan_half_vertical_fov * aspect);
        let y = point[1] / (point[2] * self.tan_half_vertical_fov);
        Pos2::new(
            rect.center().x + x * rect.width() * 0.5,
            rect.center().y - y * rect.height() * 0.5,
        )
    }

    fn project_point(self, point: EnuVector3, rect: Rect) -> Option<(Pos2, f32)> {
        let camera = self.camera_point(point);
        (camera[2] >= self.near_m).then(|| {
            let distance = dot3(camera, camera).sqrt();
            (self.screen_point(camera, rect), distance)
        })
    }

    fn project_segment(self, a: EnuVector3, b: EnuVector3, rect: Rect) -> Option<[Pos2; 2]> {
        let mut a = self.camera_point(a);
        let mut b = self.camera_point(b);
        if a[2] < self.near_m && b[2] < self.near_m {
            return None;
        }
        if a[2] < self.near_m {
            a = clip_to_depth(a, b, self.near_m);
        } else if b[2] < self.near_m {
            b = clip_to_depth(b, a, self.near_m);
        }
        Some([self.screen_point(a, rect), self.screen_point(b, rect)])
    }
}

fn clip_to_depth(behind: [f32; 3], ahead: [f32; 3], depth: f32) -> [f32; 3] {
    let t = (depth - behind[2]) / (ahead[2] - behind[2]);
    [
        behind[0] + (ahead[0] - behind[0]) * t,
        behind[1] + (ahead[1] - behind[1]) * t,
        depth,
    ]
}

fn point3(point: EnuVector3, origin: EnuVector3) -> [f32; 3] {
    [
        point.east_m - origin.east_m,
        point.north_m - origin.north_m,
        point.up_m - origin.up_m,
    ]
}

fn add(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m + right.east_m,
        left.north_m + right.north_m,
        left.up_m + right.up_m,
    )
}

fn scale(vector: EnuVector3, amount: f32) -> EnuVector3 {
    EnuVector3::new(
        vector.east_m * amount,
        vector.north_m * amount,
        vector.up_m * amount,
    )
}

#[derive(Clone, Debug)]
struct SourceTrajectory {
    waypoints: Vec<EnuVector3>,
    segment_lengths_m: Vec<f32>,
    cycle_length_m: f32,
    speed_mps: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SourceTrajectorySample {
    position: EnuVector3,
    direction: EnuVector3,
}

impl SourceTrajectory {
    fn from_fixture(trajectory: &Trajectory) -> Result<Self, String> {
        let waypoints = trajectory
            .waypoints_m
            .iter()
            .copied()
            .map(to_enu)
            .collect::<Vec<_>>();
        // Source paths are cyclic: after the final waypoint they travel along
        // the closing segment back to the first waypoint and repeat.
        let segment_lengths_m = (0..waypoints.len())
            .map(|index| {
                vector_length(subtract(
                    waypoints[(index + 1) % waypoints.len()],
                    waypoints[index],
                ))
            })
            .collect::<Vec<_>>();
        let cycle_length_m: f32 = segment_lengths_m.iter().sum();
        if !cycle_length_m.is_finite() || cycle_length_m <= 0.0 {
            return Err("source trajectory must contain a non-zero segment".into());
        }
        Ok(Self {
            waypoints,
            segment_lengths_m,
            cycle_length_m,
            speed_mps: trajectory.speed_mps as f32,
        })
    }

    fn sample_at_block(&self, elapsed_blocks: u64) -> SourceTrajectorySample {
        let elapsed_seconds =
            elapsed_blocks as f64 * f64::from(BLOCK_SIZE) / f64::from(SAMPLE_RATE);
        let mut distance_m = (elapsed_seconds * f64::from(self.speed_mps))
            .rem_euclid(f64::from(self.cycle_length_m)) as f32;
        for (index, segment_length_m) in self.segment_lengths_m.iter().copied().enumerate() {
            if segment_length_m == 0.0 {
                continue;
            }
            if distance_m < segment_length_m {
                let start = self.waypoints[index];
                let delta = subtract(self.waypoints[(index + 1) % self.waypoints.len()], start);
                let direction = scale(delta, 1.0 / segment_length_m);
                return SourceTrajectorySample {
                    position: add(start, scale(delta, distance_m / segment_length_m)),
                    direction,
                };
            }
            distance_m -= segment_length_m;
        }
        SourceTrajectorySample {
            position: self.waypoints[0],
            direction: EnuVector3::default(),
        }
    }
}

fn trajectory_segments_at_height(
    trajectory: &SourceTrajectory,
    height_m: f32,
) -> impl Iterator<Item = [EnuVector3; 2]> + '_ {
    (0..trajectory.waypoints.len()).map(move |index| {
        let mut a = trajectory.waypoints[index];
        let mut b = trajectory.waypoints[(index + 1) % trajectory.waypoints.len()];
        a.up_m = height_m;
        b.up_m = height_m;
        [a, b]
    })
}

fn subtract(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m - right.east_m,
        left.north_m - right.north_m,
        left.up_m - right.up_m,
    )
}

fn vector_length(vector: EnuVector3) -> f32 {
    (vector.east_m * vector.east_m + vector.north_m * vector.north_m + vector.up_m * vector.up_m)
        .sqrt()
}

fn to_enu(value: [f64; 3]) -> EnuVector3 {
    EnuVector3::new(value[0] as f32, value[1] as f32, value[2] as f32)
}

fn sub3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn dot3(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn cross3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn normalize3(vector: [f32; 3]) -> [f32; 3] {
    let length = dot3(vector, vector).sqrt();
    [vector[0] / length, vector[1] / length, vector[2] / length]
}

#[cfg(test)]
mod tests {
    use fightbox_api::{AssetAnalysis, AssetMeasurementProvenance, ReferenceLevel};

    use super::*;

    #[test]
    fn scene_slot_teardown_and_rebuild_never_leaks_previous_scene_ids() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let megablock = Fixture::read(&root.join("fixtures/city/megablock/fixture.json")).unwrap();
        let checkpoint = Fixture::read(&root.join("fixtures/checkpoint/fixture.json")).unwrap();

        let megablock_ids = planned_physical_source_ids(&megablock);
        let checkpoint_ids = planned_physical_source_ids(&checkpoint);
        assert_eq!(megablock_ids.len(), 5);
        // 7 = original 5 plus the A-10 gun-run pair (sky pass + strike line).
        assert_eq!(checkpoint_ids.len(), 7);
        assert!(megablock_ids.contains(&"dshk-street-gun".into()));
        assert!(checkpoint_ids.contains(&"m2-checkpoint-gun".into()));
        assert!(checkpoint_ids.contains(&"dshk-return-fire".into()));
        assert!(checkpoint_ids.contains(&"a10-gunrun-sky".into()));
        assert!(checkpoint_ids.contains(&"a10-strike-line".into()));

        let mut slots = SceneSlotState::default();
        slots.replace(megablock_ids.clone());
        slots.teardown();
        assert!(slots.active_ids.is_empty());
        slots.replace(checkpoint_ids.clone());
        assert_eq!(slots.active_ids, checkpoint_ids);
        assert!(
            slots
                .active_ids
                .iter()
                .all(|id| !megablock_ids.contains(id))
        );
        slots.teardown();
        slots.replace(megablock_ids.clone());
        assert_eq!(slots.active_ids, megablock_ids);
        assert!(
            slots
                .active_ids
                .iter()
                .all(|id| !checkpoint_ids.contains(id))
        );
    }

    #[test]
    fn checkpoint_autopilot_uses_the_fixture_loop_and_walking_speed() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let fixture = Fixture::read(&root.join("fixtures/checkpoint/fixture.json")).unwrap();
        let mut autopilot = Autopilot::for_scene(
            Bounds2 {
                min: [0.0, 0.0],
                max: [585.0, 585.0],
            },
            &fixture,
            "checkpoint-block",
            EnuVector3::new(197.5, 292.5, 1.5),
        );
        assert!(autopilot.enabled);
        assert_eq!(autopilot.speed_mps, 1.5);
        assert_eq!(autopilot.circuit.min, [197.5, 292.5]);
        assert_eq!(autopilot.circuit.max, [292.5, 387.5]);
        let sample = autopilot.advance(1.0);
        assert_eq!(sample.position, [199.0, 292.5]);
        assert_eq!(sample.direction, [1.0, 0.0]);
    }
    use std::sync::{Arc, Mutex};

    struct RecordingProcessor {
        listener: ListenerState,
        observed: Arc<Mutex<Vec<ListenerState>>>,
        safety: SafetyTelemetry,
    }

    impl ListenerStateSink for RecordingProcessor {
        fn set_listener_state(&mut self, listener: ListenerState) {
            self.listener = listener;
        }
    }

    impl BlockProcessor for RecordingProcessor {
        fn block_size_frames(&self) -> usize {
            1
        }

        fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
            self.observed.lock().unwrap().push(self.listener);
            block.output_left[0] = 0.0;
            block.output_right[0] = 0.0;
            Ok(())
        }

        fn safety_telemetry(&self) -> SafetyTelemetry {
            self.safety
        }
    }

    #[test]
    fn listener_orientation_is_late_bound_for_each_audio_block() {
        let north = ListenerControl::at(
            EnuVector3::new(0.0, 0.0, 1.5),
            EnuVector3::new(0.0, 1.0, 0.0),
        )
        .listener_state(EnuVector3::default());
        let east = ListenerControl::at(
            EnuVector3::new(0.0, 0.0, 1.5),
            EnuVector3::new(1.0, 0.0, 0.0),
        )
        .listener_state(EnuVector3::default());
        let (mut mailbox, reader) = PoseMailbox::new(north);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let processor = RecordingProcessor {
            listener: north,
            observed: Arc::clone(&observed),
            safety: SafetyTelemetry {
                proximity_ceiling_engagements: 3,
                limiter_engagements: 2,
                pre_limiter_peak: 1.2,
                post_limiter_peak: 0.8,
            },
        };
        let (meter_writer, _meter_reader) = SnapshotPublication::new(MeterReading::SILENT);
        let (audio_block_writer, _audio_block_reader) = SnapshotPublication::new(0_u64);
        let mut late = LateBoundProcessor::new(
            processor,
            reader,
            meter_writer,
            MeterAccumulator::new(48_000, 1, 0.5),
            audio_block_writer,
            None,
        );
        let mut left = [0.0];
        let mut right = [0.0];
        let mut render = |processor: &mut LateBoundProcessor<RecordingProcessor>| {
            processor
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &[],
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
        };
        render(&mut late);
        mailbox.publish(east);
        render(&mut late);
        assert_eq!(*observed.lock().unwrap(), vec![north, east]);
        assert_eq!(
            late.safety_telemetry(),
            SafetyTelemetry {
                proximity_ceiling_engagements: 3,
                limiter_engagements: 2,
                pre_limiter_peak: 1.2,
                post_limiter_peak: 0.8,
            }
        );
    }

    #[test]
    fn mesh_faces_cache_normals_and_distinguish_ground() {
        let mesh = AcousticMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, 0.0, 0.0),
                EnuVector3::new(1.0, 0.0, 0.0),
                EnuVector3::new(1.0, 1.0, 0.0),
                EnuVector3::new(1.0, 1.0, 3.0),
            ],
            triangles: vec![[0, 1, 2], [1, 3, 2]],
            material_ids: vec![0, 0],
        };
        let faces = mesh_faces(&mesh);
        assert_eq!(faces.len(), 2);
        assert!(faces[0].is_ground);
        assert!(!faces[1].is_ground);
        assert_eq!(faces[0].normal, [0.0, 0.0, 1.0]);
        assert_eq!(faces[1].normal, [-1.0, 0.0, 0.0]);
        assert_ne!(face_color(faces[0]), face_color(faces[1]));
    }

    #[test]
    fn painter_depth_key_orders_far_faces_before_near_faces() {
        let near = [[0.0, 0.0, 2.0], [1.0, 0.0, 2.0], [0.0, 1.0, 2.0]];
        let far = [[0.0, 0.0, 9.0], [1.0, 0.0, 9.0], [0.0, 1.0, 9.0]];
        let mut projected = [
            ProjectedFace {
                points: [Pos2::ZERO; 4],
                point_count: 3,
                depth: polygon_depth(&near),
                fill: Color32::RED,
            },
            ProjectedFace {
                points: [Pos2::ZERO; 4],
                point_count: 3,
                depth: polygon_depth(&far),
                fill: Color32::BLUE,
            },
        ];
        let mesh = projected_faces_mesh(&mut projected);
        assert_eq!(projected[0].depth, 9.0);
        assert_eq!(projected[1].depth, 2.0);
        assert_eq!(mesh.vertices[0].color, Color32::BLUE);
        assert_eq!(mesh.vertices[3].color, Color32::RED);
    }

    #[test]
    fn face_shading_stays_lit_and_varies_by_orientation() {
        let roof = face_brightness([0.0, 0.0, 1.0]);
        let wall = face_brightness([1.0, 0.0, 0.0]);
        assert!((0.46..=1.0).contains(&roof));
        assert!((0.46..=1.0).contains(&wall));
        assert!(roof > wall);
    }

    #[test]
    fn face_crossing_near_plane_is_clipped_to_a_quad() {
        let triangle = [[-1.0, 0.0, 1.0], [1.0, 0.0, 1.0], [0.0, 1.0, 0.0]];
        let clipped = clip_polygon_to_near(&triangle, 0.5);
        assert_eq!(clipped.point_count, 4);
        assert!(
            clipped.points[..clipped.point_count]
                .iter()
                .all(|point| point[2] >= 0.5)
        );
    }

    #[test]
    fn city_sized_filled_geometry_projection_stays_interactive() {
        let mut mesh = AcousticMesh {
            vertices_enu_m: Vec::new(),
            triangles: Vec::new(),
            material_ids: Vec::new(),
        };
        for index in 0..2_048 {
            let column = (index % 64) as f32;
            let row = (index / 64) as f32;
            let left = column * 2.0 - 64.0;
            let right = left + 1.5;
            let north = row * 3.0 + 8.0;
            let base = mesh.vertices_enu_m.len() as u32;
            mesh.vertices_enu_m.extend([
                EnuVector3::new(left, north, 0.0),
                EnuVector3::new(right, north, 0.0),
                EnuVector3::new(right, north, 8.0),
                EnuVector3::new(left, north, 8.0),
            ]);
            mesh.triangles
                .extend([[base, base + 1, base + 2], [base, base + 2, base + 3]]);
            mesh.material_ids.extend([0, 0]);
        }
        let faces = mesh_faces(&mesh);
        let projection = FirstPersonProjection::new(
            EnuVector3::new(0.0, 0.0, 1.5),
            0.0,
            FIRST_PERSON_VERTICAL_FOV_RADIANS,
            FIRST_PERSON_NEAR_M,
        );
        let rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(1_280.0, 720.0));
        let started = Instant::now();
        let mut index_count = 0;
        for _ in 0..30 {
            let mut projected = faces
                .iter()
                .filter_map(|face| {
                    project_face(
                        &mesh,
                        *face,
                        rect,
                        |point| projection.camera_point(point),
                        |point, rect| projection.screen_point(point, rect),
                    )
                })
                .collect::<Vec<_>>();
            index_count = projected_faces_mesh(&mut projected).indices.len();
        }
        let elapsed = started.elapsed();
        eprintln!(
            "4,096-face projection, sort, and mesh assembly: {:.2} ms/frame",
            elapsed.as_secs_f64() * 1_000.0 / 30.0
        );
        assert_eq!(index_count, 4_096 * 3);
    }

    #[test]
    fn every_source_height_selector_uses_the_same_three_options() {
        assert_eq!(
            SourceHeight::ALL,
            [
                SourceHeight::Street,
                SourceHeight::Medium,
                SourceHeight::AboveRooves,
            ]
        );
        assert_eq!(
            SourceHeight::ALL.map(SourceHeight::label),
            ["street", "medium", "roofline +3 m"]
        );
    }

    #[test]
    fn the_raised_height_label_quotes_the_clearance_it_applies() {
        let levels = SourceHeightLevels {
            tallest_roof_m: 84.0,
        };

        assert_eq!(
            SourceHeight::AboveRooves.label(),
            format!("roofline +{ROOFLINE_CLEARANCE_M} m")
        );
        assert_eq!(
            levels.height_m(SourceHeight::AboveRooves, 1.5),
            84.0 + ROOFLINE_CLEARANCE_M
        );
    }

    #[test]
    fn the_raised_height_label_does_not_disturb_the_saved_sidecar_token() {
        let saved = SourceHeightDefault::from(SourceHeight::AboveRooves);

        assert_eq!(serde_json::to_string(&saved).unwrap(), r#""above_rooves""#);
    }

    #[test]
    fn source_height_levels_scan_the_tallest_mesh_roof() {
        let mesh = AcousticMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, 0.0, 0.0),
                EnuVector3::new(1.0, 0.0, 18.0),
                EnuVector3::new(1.0, 1.0, 72.5),
                EnuVector3::new(0.0, 1.0, 31.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            material_ids: vec![0, 0],
        };

        assert_eq!(SourceHeightLevels::for_mesh(&mesh).tallest_roof_m, 72.5);
    }

    #[test]
    fn source_height_levels_map_medium_and_above_rooves_from_the_tallest_roof() {
        let levels = SourceHeightLevels {
            tallest_roof_m: 84.0,
        };

        assert_eq!(levels.height_m(SourceHeight::Medium, 1.5), 42.0);
        assert_eq!(levels.height_m(SourceHeight::AboveRooves, 1.5), 87.0);
    }

    #[test]
    fn street_height_restores_the_fixture_declared_height_exactly() {
        let levels = SourceHeightLevels {
            tallest_roof_m: 84.0,
        };
        let fixture_declared_height_m = f32::from_bits(0x3fca_8642);

        assert_eq!(
            levels.height_m(SourceHeight::Medium, fixture_declared_height_m),
            42.0
        );
        assert_eq!(
            levels.height_m(SourceHeight::Street, fixture_declared_height_m),
            fixture_declared_height_m
        );
    }

    #[test]
    fn free_field_spl_uses_current_distance_and_bounds_the_source_radius() {
        let source = EnuVector3::new(0.0, 0.0, 0.0);
        assert_eq!(
            free_field_spl_at_listener_db(120.0, source, EnuVector3::new(1.0, 0.0, 0.0), 1.0,),
            120.0
        );
        assert!(
            (free_field_spl_at_listener_db(120.0, source, EnuVector3::new(10.0, 0.0, 0.0), 1.0,)
                - 100.0)
                .abs()
                < 1.0e-5
        );
        assert_eq!(
            free_field_spl_at_listener_db(120.0, source, EnuVector3::new(0.1, 0.0, 0.0), 1.0,),
            120.0
        );
    }

    #[test]
    fn workbench_output_safety_setup_publishes_source_and_listener_geometry() {
        let profile = SourceProfile {
            id: SourceId::new("hot-source"),
            pose: Pose {
                position: EnuVector3::new(0.0, 0.0, 0.0),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            reference_level: fightbox_api::ReferenceLevel::SplAtOneMeter { db_spl: 155.0 },
            asset_analysis: fightbox_api::AssetAnalysis::new(
                -24.0,
                -12.0,
                fightbox_api::AssetMeasurementProvenance::new("workbench-safety-test/v1").unwrap(),
            )
            .unwrap(),
            extent: fightbox_api::ExtentDescriptor::Point,
            directivity: fightbox_api::Directivity::default(),
            max_speed_mps: 0.0,
        };
        let listener_position = EnuVector3::new(1.0, 0.0, 0.0);
        let (mut safety_controller, safety_reader) =
            configure_output_safety(listener_position, std::slice::from_ref(&profile)).unwrap();
        safety_controller.set_monitor_gain_db(-6.0).unwrap();

        let propagation = PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        };
        let (_writer, propagation_reader) = SnapshotPublication::new(propagation);
        let config = EngineConfig {
            block_size_frames: 64,
            max_active_sources: 1,
            ..EngineConfig::default()
        };
        let mut graph =
            RuntimeGraph::new_with_output_safety(config, propagation_reader, safety_reader)
                .unwrap();
        graph
            .set_source(0, &profile, SceneCalibration::default())
            .unwrap();
        let input = [0.001_f32; 64];
        let source_blocks = [fightbox_runtime::SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [0.0_f32; 64];
        let mut right = [0.0_f32; 64];
        graph
            .process_block(ProcessBlock {
                now_ns: 0,
                sources: &source_blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        assert_eq!(graph.safety_telemetry().proximity_ceiling_engagements, 1);
    }

    #[test]
    fn enable_mute_and_solo_gain_matrix_is_source_local_and_silence_wins() {
        let mut mix = SourceMix::ALL_AUDIBLE;
        assert_eq!(&mix.gains(3)[..3], &[1.0, 1.0, 1.0]);

        mix.enabled[0] = false;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 1.0, 1.0]);

        mix.muted[1] = true;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 1.0]);

        mix.soloed[2] = true;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 1.0]);

        mix.soloed[1] = true;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 1.0]);

        mix.enabled[2] = false;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 0.0]);
    }

    #[test]
    fn disabled_solo_does_not_silence_enabled_sources() {
        let mut mix = SourceMix::ALL_AUDIBLE;
        mix.enabled[1] = false;
        mix.soloed[1] = true;
        assert_eq!(&mix.gains(3)[..3], &[1.0, 0.0, 1.0]);
    }

    #[test]
    fn monitor_offset_is_source_local_and_does_not_change_calibrated_descriptor_level() {
        let profile = SourceProfile {
            id: SourceId::new("calibrated-source"),
            pose: Pose {
                position: EnuVector3::default(),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            reference_level: ReferenceLevel::SplAtOneMeter { db_spl: 155.0 },
            asset_analysis: AssetAnalysis::new(
                -24.0,
                -12.0,
                AssetMeasurementProvenance::new("monitor-offset-test/v1").unwrap(),
            )
            .unwrap(),
            extent: fightbox_api::ExtentDescriptor::Point,
            directivity: fightbox_api::Directivity::default(),
            max_speed_mps: 0.0,
        };
        let _descriptor = MultiSourceDescriptor::at(profile.pose.position)
            .with_reference_level(profile.reference_level);
        let mut mix = SourceMix::ALL_AUDIBLE;
        mix.monitor_gains[0] = monitor_offset_gain(-6.0);

        assert!((mix.gains(2)[0] - 10.0_f32.powf(-6.0 / 20.0)).abs() < 1.0e-6);
        assert_eq!(mix.gains(2)[1], 1.0);
        assert_eq!(
            profile.reference_level,
            ReferenceLevel::SplAtOneMeter { db_spl: 155.0 }
        );
    }

    #[test]
    fn compact_level_truth_formats_base_offset_and_effective_level() {
        assert_eq!(format_level_truth(155.0, -6.0), "155 -6 -> 149 dB SPL");
        assert_eq!(format_level_truth(105.0, 0.0), "105 +0 -> 105 dB SPL");
        assert_eq!(format_level_truth(118.0, 2.5), "118 +2.5 -> 120.5 dB SPL");
    }

    #[test]
    fn stage_mix_all_on_bypass_and_solo_resolve_to_one_atomic_gain_snapshot() {
        assert_eq!(StageMix::ALL_ENABLED.gains(), StageOutputGains::UNITY);

        let mut mix = StageMix::ALL_ENABLED;
        mix.bypassed[0] = true;
        assert_eq!(
            mix.gains(),
            StageOutputGains {
                direct: 0.0,
                pathing: 1.0,
                reflections: 1.0,
            }
        );

        mix.soloed[2] = true;
        assert_eq!(
            mix.gains(),
            StageOutputGains {
                direct: 0.0,
                pathing: 0.0,
                reflections: 1.0,
            }
        );

        mix.bypassed[2] = true;
        assert_eq!(
            mix.gains(),
            StageOutputGains {
                direct: 0.0,
                pathing: 1.0,
                reflections: 0.0,
            },
            "a bypassed solo must not silence the remaining audible stage"
        );
    }

    #[test]
    fn source_trajectory_position_is_determined_by_elapsed_audio_blocks() {
        let trajectory = SourceTrajectory::from_fixture(&Trajectory {
            waypoints_m: vec![[0.0, 0.0, 1.5], [10.0, 0.0, 1.5], [10.0, 10.0, 1.5]],
            speed_mps: 2.0,
            max_speed_mps: Some(2.0),
        })
        .unwrap();

        let after_one_second = trajectory.sample_at_block(375);
        assert_eq!(after_one_second.position, EnuVector3::new(2.0, 0.0, 1.5));
        assert_eq!(after_one_second.direction, EnuVector3::new(1.0, 0.0, 0.0));
        let at_first_corner = trajectory.sample_at_block(1_875);
        assert_eq!(at_first_corner.position, EnuVector3::new(10.0, 0.0, 1.5));
        assert_eq!(at_first_corner.direction, EnuVector3::new(0.0, 1.0, 0.0));
        assert_eq!(
            trajectory.sample_at_block(375),
            trajectory.sample_at_block(375)
        );
    }

    #[test]
    fn meter_accumulates_peak_and_rms_over_rolling_window() {
        let mut meter = MeterAccumulator::new(4, 2, 1.0);
        let first = meter.observe(&[1.0, 0.0], &[0.0, 0.0]);
        assert_eq!(first.peak_dbfs, 0.0);
        assert!((first.rms_dbfs - -6.020_600_3).abs() < 1.0e-5);
        let second = meter.observe(&[0.5, 0.5], &[0.5, 0.5]);
        assert_eq!(second.peak_dbfs, 0.0);
        let third = meter.observe(&[0.0, 0.0], &[0.0, 0.0]);
        assert!((third.peak_dbfs - -6.020_600_3).abs() < 1.0e-5);
        assert!((third.rms_dbfs - -9.030_9).abs() < 1.0e-4);
    }

    #[test]
    fn autopilot_derives_inset_rectangle_and_moves_at_constant_speed() {
        let circuit = Bounds2 {
            min: [0.0, 0.0],
            max: [100.0, 60.0],
        }
        .inset_circuit();
        assert!((circuit.min[0] - 9.6).abs() < 1.0e-5);
        assert!((circuit.min[1] - 9.6).abs() < 1.0e-5);
        assert!((circuit.max[0] - 90.4).abs() < 1.0e-5);
        assert!((circuit.max[1] - 50.4).abs() < 1.0e-5);
        let start = circuit.sample(0.0);
        assert!((start.position[0] - 9.6).abs() < 1.0e-5);
        assert!((start.position[1] - 9.6).abs() < 1.0e-5);
        assert_eq!(start.direction, [1.0, 0.0]);
        let corner = circuit.sample(80.8).position;
        assert!((corner[0] - 90.4).abs() < 1.0e-5);
        assert!((corner[1] - 9.6).abs() < 1.0e-5);
        let northbound = circuit.sample(90.8).position;
        assert!((northbound[0] - 90.4).abs() < 1.0e-5);
        assert!((northbound[1] - 19.6).abs() < 1.0e-5);
        let a = circuit.sample(25.0).position;
        let b = circuit.sample(31.0).position;
        assert!(((b[0] - a[0]).hypot(b[1] - a[1]) - 6.0).abs() < 1.0e-6);
    }

    #[test]
    fn first_person_projection_places_known_points_and_clips_near_plane() {
        let projection = FirstPersonProjection::new(
            EnuVector3::new(0.0, 0.0, 1.5),
            0.0,
            FIRST_PERSON_VERTICAL_FOV_RADIANS,
            FIRST_PERSON_NEAR_M,
        );
        let rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(200.0, 100.0));
        let (center, distance) = projection
            .project_point(EnuVector3::new(0.0, 10.0, 1.5), rect)
            .unwrap();
        assert!((center.x - 100.0).abs() < 1.0e-6);
        assert!((center.y - 50.0).abs() < 1.0e-6);
        assert!((distance - 10.0).abs() < 1.0e-6);
        let right = projection
            .project_point(EnuVector3::new(1.0, 10.0, 1.5), rect)
            .unwrap()
            .0;
        assert!(right.x > center.x);
        assert!(
            projection
                .project_point(EnuVector3::new(0.0, -1.0, 1.5), rect)
                .is_none()
        );
        assert!(
            projection
                .project_segment(
                    EnuVector3::new(0.0, -1.0, 1.5),
                    EnuVector3::new(0.0, 1.0, 1.5),
                    rect,
                )
                .is_some()
        );
    }

    #[test]
    fn picture_in_picture_is_top_right_and_bounded_by_the_main_view() {
        let main = Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(1010.0, 620.0));
        let pip = picture_in_picture_rect(main);
        assert_eq!(pip.right(), main.right() - PICTURE_IN_PICTURE_MARGIN);
        assert_eq!(pip.top(), main.top() + PICTURE_IN_PICTURE_MARGIN);
        assert!(main.contains_rect(pip));

        let small_main = Rect::from_min_max(Pos2::ZERO, Pos2::new(200.0, 100.0));
        assert!(small_main.contains_rect(picture_in_picture_rect(small_main)));
    }

    #[test]
    fn artillery_one_shot_retriggers_on_sample_clock_and_rearms_after_disable() {
        assert_eq!(artillery_retrigger_frames(48_000), 144_000);
        let configured = SourcePlayback::for_asset(ARTILLERY_ASSET_ID, 48_000);
        assert_eq!(
            configured.mode,
            PlaybackMode::PeriodicOneShot {
                interval_frames: 144_000
            }
        );
        assert_eq!(
            SourcePlayback::for_asset("toms-diner", 48_000).mode,
            PlaybackMode::Looping
        );
        let mut playback = SourcePlayback {
            mode: PlaybackMode::PeriodicOneShot { interval_frames: 4 },
            cursor: 0,
            was_enabled: false,
        };
        let signal = [1.0, 0.5];
        let enabled = (0..6)
            .map(|_| playback.next_sample(&signal, true))
            .collect::<Vec<_>>();
        assert_eq!(enabled, vec![1.0, 0.5, 0.0, 0.0, 1.0, 0.5]);
        assert_eq!(playback.next_sample(&signal, false), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
    }
}
