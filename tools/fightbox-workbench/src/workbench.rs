use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Instant;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke};
use fightbox_api::{
    EngineConfig, EnuVector3, ListenerState, OutputSafetyConfig, Pose, SceneCalibration, SourceId,
    SourceProfile,
};
use fightbox_runtime::backend::{SimulationUpdate, SourceMotion};
use fightbox_runtime::{
    BlockProcessor, CallbackTimingPublication, CallbackTimingWriter, MonitorRouteController,
    MonitorRoutePublication, OutputSafetyController, OutputSafetyPublication, OutputSafetyReader,
    ProcessBlock, PropagationSnapshot, RAW_MONITOR_PAD_DB, RenderError, RuntimeGraph,
    SafetyTelemetry, SimulationCadences, SimulationWorker, SnapshotPublication, SnapshotReader,
    SnapshotWriter, SourcePropagation,
};
use fightbox_steam_audio::{
    AudioConfig, BakedProbeBatch, DirectOcclusionMode, MultiSourceDescriptor, S3SimulationConfig,
    SceneMesh, SceneAirControl, StageOutputGainControl, StageOutputGains, build_multi_source_session,
};
use fightbox_world::{AcousticMesh, LoadedPackage, read_package};

use crate::{LaunchArgs, RenderFormat};
use crate::acoustic_state::{
    AcousticTelemetry, AcousticTelemetryTap, BadgeTextCache, BadgeTone, ProbeCoverageQuery,
    SourceAcousticInputs, SourceAcousticState,
};
use crate::anomaly_field::{
    FieldContext, FieldController, FieldIdentity, SourceQuery, source_query,
};
use crate::asset::{PreparedAsset, load_asset, prepare_song};
use crate::song_program::{SongBuffer, SongReader, SongWriter, song_channel};
use crate::ballistic_crack::{BallisticCrack, CrackPlayback};
use crate::capture::{
    BakeProvenance, BrowserScan, CaptureBrowserEntry, CaptureController, CaptureDraft,
    CaptureEndStats, CaptureEngineConfig, CaptureQualitySettings, CaptureSourceState, CaptureTap,
    WorldPackageProvenance, default_capture_root, git_identity, json_string_field,
    reveal_in_finder, scan_capture_bundles, sha256_file, utc_timestamp_now,
};
use crate::fixture::{
    AirPreset, Fixture, FixtureAir, Trajectory, VisibilityRangeAdoption, load_baked, occlusion_mode_for_extent, scene_mesh,
};
use crate::mix_defaults::{
    MAX_MONITOR_GAIN_DB, MAX_SOURCE_OFFSET_DB, MIN_MONITOR_GAIN_DB, MIN_SOURCE_OFFSET_DB,
    MixDefaults, SourceHeightDefault, SourceMixDefault, clamp_source_offset_db,
};
use crate::head_tracking::HeadTracking;
use crate::pose::{ListenerControl, PoseMailbox};
use crate::scene::{SceneControl, SceneStatus, SceneTimeline};
use crate::quiet_output::{QUIET_CEILING_DBFS, QuietOutputGuard, QuietOutputReader};

#[cfg(all(test, feature = "linked-sdk", feature = "live-output"))]
#[path = "combat_profile.rs"]
mod combat_profile;

const BLOCK_SIZE: u32 = 128;
const SAMPLE_RATE: u32 = 48_000;
/// How far from a speaker the City Map's music field is routed.
const MUSIC_FIELD_RADIUS_M: f32 = 420.0;
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

/// Conservative screen-space margin for face frustum rejection. A face is
/// dropped only when its whole projected bounding box lies beyond the same
/// expanded-rect boundary by this margin, far more than any anti-aliasing or
/// feathering footprint, so a dropped face can never have contributed pixels.
const FACE_CULL_MARGIN_PX: f32 = 48.0;

/// Constant hover copy hoisted out of the per-frame panel build; the `format!`
/// below runs once instead of once per source per repaint.
static RAW_A_B_ROUTE_HOVER: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Decoded mono in both ears at {RAW_MONITOR_PAD_DB:.0} dB before the current monitor gain; \
         bypasses source drive, distance, HRTF, occlusion, pathing, and reflections. \
         The final limiter remains active."
    )
});

static ARTILLERY_RETRIGGER_LABEL: LazyLock<String> =
    LazyLock::new(|| format!("{ARTILLERY_RETRIGGER_SECONDS} s retrigger"));
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
    quiet_guard: Option<QuietOutputGuard>,
    /// Opt-in City Map link (`FIGHTBOX_MAP_LINK`); kept across scene switches.
    map_link: Option<crate::map_link::MapLink>,
}

impl WorkbenchApp {
    pub fn load(args: LaunchArgs, startup_started: Instant) -> Result<Self, String> {
        if args.quiet_audition.is_some()
            && (!args.start_audio
                || args
                    .device
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty()))
        {
            return Err(
                "quiet audition requires explicit --start-audio and an exact --device".into(),
            );
        }
        if args
            .replay
            .as_ref()
            .is_some_and(|replay| !(-20..=40).contains(&replay.monitor_gain_db))
        {
            return Err("replay monitor gain must be in -20..=40 dB".into());
        }
        if args.replay.as_ref().is_some_and(|r| r.pin_full_quality) && !args.null_output {
            return Err("Full quality pin requires headless null output".into());
        }
        // One budget per launch, including every subsequent scene rebuild/restore.
        let quiet_guard = args
            .quiet_audition
            .as_ref()
            .map(|options| QuietOutputGuard::new(options.seconds, SAMPLE_RATE))
            .transpose()
            .map_err(str::to_owned)?;
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
                    .filter(|source| source.live_input.is_none() && source.program_file.is_none())
                    .map(|source| source.asset_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let mut assets = asset_ids
            .into_iter()
            .map(|asset_id| load_asset(&asset_id).map(|asset| (asset_id, asset)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        // Prepare declared personal files before the GUI exists; tab rebuilds
        // reuse these decoded assets instead of decoding on the UI thread.
        for scene in &scenes {
            for source in &scene.fixture.sources {
                if let Some(path) = source.song_file() {
                    let (key, path) = song_asset_key(&scene.path, path, source.extent);
                    if !assets.contains_key(&key) {
                        let stereo = matches!(source.extent, fightbox_api::ExtentDescriptor::StereoImage { .. });
                        assets.insert(key, prepare_song(&path, stereo)?);
                    }
                }
            }
        }
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
            quiet_guard.clone(),
        )?;
        let mut slots = SceneSlotState::default();
        slots.replace(active.physical_source_ids());
        let map_link = match crate::map_link::MapLink::from_env() {
            Some(Ok(link)) => {
                eprintln!("[map link] City Map link listening on {}", link.address);
                Some(link)
            }
            Some(Err(error)) => {
                eprintln!("[map link] off: {error}");
                None
            }
            None => None,
        };
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
            quiet_guard,
            map_link,
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
            self.quiet_guard.clone(),
        );
        if let Some(link) = &mut self.map_link {
            link.regreet();
        }
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
                    self.quiet_guard.clone(),
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

/// Per-scene City Map link state; the socket itself lives on the app.
#[derive(Default)]
struct MapLinkState {
    hello: Option<String>,
    /// The hello's suggested spots; `spot` requests pick from these by key.
    spots: Vec<crate::map_link::Spot>,
    /// Level trims raised by a loud spot: source index to (trim before,
    /// boosted trim). Any later map move of that sound puts it back.
    boosts: BTreeMap<usize, (f32, f32)>,
    /// Acoustic-feed events already sent: source index to (sequence, trigger).
    shots_sent: BTreeMap<usize, (u64, u64)>,
    /// Footprints from the scene geojson; a tap inside one never moves You.
    footprints: Vec<Vec<crate::ground_map::Point>>,
    /// The hello's walkable dots, `[east, north, quality]`.
    hello_dots: Vec<[f32; 3]>,
    /// Music field per source: the source position it was routed from and
    /// its `field` line; `field_sent` marks the lines every client has.
    fields: BTreeMap<usize, (EnuVector3, String)>,
    field_sent: BTreeMap<usize, EnuVector3>,
    /// Music paths last sent: source index to (source, listener) positions.
    paths_sent: BTreeMap<usize, (EnuVector3, EnuVector3)>,
    last_paths: Option<Instant>,
    /// A song changed: send every `track` again.
    tracks_dirty: bool,
    last_state: String,
    last_sent: Option<Instant>,
    drag: Option<MapDrag>,
}

struct MapDrag {
    index: usize,
    planned: EnuVector3,
    last_plan: Instant,
}

struct SongLoad {
    index: usize,
    path: PathBuf,
    receiver: std::sync::mpsc::Receiver<Result<PreparedAsset, String>>,
}

fn song_asset_key(fixture: &std::path::Path, file: &str, extent: fightbox_api::ExtentDescriptor) -> (String, PathBuf) {
    let file = PathBuf::from(file);
    let path = if file.is_absolute() { file } else { fixture.parent().unwrap_or(std::path::Path::new(".")).join(file) };
    let stereo = matches!(extent, fightbox_api::ExtentDescriptor::StereoImage { .. });
    (format!("program-file:{}:{stereo}", path.display()), path)
}

fn adapt_song_to_slot(asset: &mut PreparedAsset, slot_rms_dbfs: f32) {
    // Preserve the existing graph calibration without adding another drive.
    let gain = 10.0_f32.powf((slot_rms_dbfs - asset.analysis.program_rms_dbfs) / 20.0);
    for sample in &mut asset.samples { *sample *= gain; }
    if let Some(stereo) = &mut asset.stereo_samples {
        for frame in stereo { frame[0] *= gain; frame[1] *= gain; }
    }
}

fn song_file_label(path: &std::path::Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

/// "toms-diner-48k-mono.wav" reads as "Toms diner".
fn song_name(path: &std::path::Path) -> String {
    let stem = path.file_stem().unwrap_or(path.as_os_str()).to_string_lossy();
    let words = stem
        .split(['-', '_', ' '])
        .filter(|word| {
            !word.is_empty()
                && !matches!(word.to_ascii_lowercase().as_str(), "48k" | "44k" | "mono" | "stereo")
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_else(|| stem.into_owned())
}

fn song_target(sources: &[SourceView], hovered: Option<usize>) -> Option<usize> {
    hovered.filter(|index| *index < sources.len())
        .or_else(|| sources.iter().position(|source| source.song_fallback))
        .or_else(|| (!sources.is_empty()).then_some(0))
}

fn newer_play(generation: u64, loaded_generation: u64) -> bool {
    let distance = generation.wrapping_sub(loaded_generation);
    distance != 0 && distance < (1 << 63)
}

pub struct Workbench {
    mesh: AcousticMesh,
    faces: Vec<MeshFace>,
    /// Static per-face shading, computed once when the mesh is loaded. Face
    /// color depends only on `MeshFace::{normal, is_ground}`, both fixed at
    /// scene load, so no runtime invalidation is needed.
    face_colors: Vec<Color32>,
    sources: Vec<SourceView>,
    /// Gains implied by `SourceMix::from_sources(&self.sources)`, cached
    /// between control ticks and invalidated at every mix mutation boundary
    /// (the three `source_mix_writer.publish(SourceMix::from_sources(..))`
    /// sites). `None` forces a recompute on the next tick.
    mix_gains_cache: Option<[f32; fightbox_runtime::MAX_ACTIVE_SOURCES]>,
    /// Overlay `FieldIdentity` memoized on its only per-frame inputs: selected
    /// source index, that source's position, the listener height feeding the
    /// grid, and the runtime-mutable grid spacing (bit-exact). Every other
    /// identity input — schema, mesh/material/bake/fixture hashes, grid bounds,
    /// source SPL, descriptor, asset identity, and simulation settings — is
    /// immutable after startup.
    overlay_identity_cache: Option<((usize, EnuVector3, f32, u32), FieldIdentity)>,
    listener: ListenerControl,
    head_tracking: HeadTracking,
    pose_mailbox: PoseMailbox,
    simulation: SimulationWorker,
    source_motion: [SourceMotion; fightbox_runtime::MAX_ACTIVE_SOURCES],
    /// Crack companion slots after the ordinary sources, one per ballistic
    /// fixture source. They are outside `sources`, so solo and mute follow
    /// the owning source instead of applying to the slot itself.
    ballistic_cracks: Vec<BallisticCrack>,
    /// Host-planned echoes of one-shot impulsive sources, replanned on Listen.
    host_echoes: Option<crate::echo_paths::HostEchoes>,
    feed_planner: crate::echo_paths::EchoPathPlanner,
    feed_writer: SnapshotWriter<Option<crate::acoustic_feed::AcousticEvent>>,
    feed_reader: SnapshotReader<Option<crate::acoustic_feed::AcousticEvent>>,
    feed_events: Vec<crate::acoustic_feed::AcousticEvent>,
    feed_history: Vec<crate::acoustic_feed::AcousticEvent>,
    feed_sequences: [u64; fightbox_runtime::MAX_ACTIVE_SOURCES],
    feed_audio_sample: u64,
    feed_contexts: Vec<Option<(u64, EnuVector3, EnuVector3)>>,
    audio: AudioState,
    pending_audio: Option<Box<dyn FnOnce() -> AudioState>>,
    // Keep bank owners alive until both output and deferred callback are destroyed.
    song_writers: Vec<SongWriter>,
    song_load: Option<SongLoad>,
    song_status: Option<String>,
    /// First live-input speaker, which also plays songs; and each slot's last song.
    music_speaker: Option<usize>,
    speaker_songs: Vec<Option<PathBuf>>,
    drop_markers: Vec<(usize, Pos2)>,
    quiet_output: Option<QuietOutputReader>,
    camera: Camera,
    monitor_gain_db: f32,
    scene_air: FixtureAir,
    scene_air_exponents: [f32; 3],
    scene_air_control: SceneAirControl,
    output_safety_controller: OutputSafetyController,
    meter_reader: SnapshotReader<MeterReading>,
    source_mix_writer: SnapshotWriter<SourceMix>,
    scene_control: SceneControl,
    scene_control_writer: SnapshotWriter<SceneControl>,
    scene_cues: Vec<crate::fixture::FixtureCue>,
    scene_prepared_listener: Option<EnuVector3>,
    playback_status_reader: SnapshotReader<PlaybackSnapshot>,
    level_trace: crate::level_trace_ui::LevelTraceUi,
    trace_control_writer: SnapshotWriter<TraceControl>,
    trace_generation: u64,
    trace_config_epoch: u64,
    trace_last_config: Option<TraceUiConfig>,
    trace_recording_snapshots: VecDeque<serde_json::Value>,
    trace_export_status: Option<String>,
    monitor_route_controller: MonitorRouteController,
    source_comparison: Option<SourceComparison>,
    stage_mix: StageMix,
    stage_output_gain_control: StageOutputGainControl,
    audio_block_reader: SnapshotReader<u64>,
    capture: CaptureController,
    ambix_capture: Option<std::sync::Arc<crate::spatial_export::StemCapture>>,
    capture_state: CaptureUiState,
    capture_static: CaptureStaticContext,
    capture_entries: Vec<CaptureBrowserEntry>,
    capture_warnings: Vec<String>,
    capture_status: Option<String>,
    fixture_path: PathBuf,
    scene_positions: crate::scene_positions::ScenePositions,
    scene_save_status: Option<String>,
    saved_fixture: Option<Fixture>,
    source_drag: Option<crate::source_drag::SourceDrag>,
    mix_defaults_status: Option<String>,
    autopilot: Autopilot,
    source_height_levels: SourceHeightLevels,
    probe_coverage: ProbeCoverageQuery,
    probe_points: Vec<[f32; 3]>,
    map: MapLinkState,
    acoustic_telemetry: SnapshotReader<AcousticTelemetry>,
    live_stage_energy: SnapshotReader<fightbox_steam_audio::LiveStageEnergySnapshot>,
    anomaly_field: FieldController,
    listening_mode: bool,
    ground_map_enabled: bool,
    ground_map_whole_scene: bool,
    ground_map_local_frame: crate::ground_map::LocalSoundFrame,
    ground_map: crate::ground_map::GroundMap,
    visibility_range: VisibilityRangeAdoption,
    startup_started: Instant,
    reflection_warmup_started: Instant,
    reflection_warmup_reported: bool,
    first_frame_reported: bool,
    output_device_label: String,
    audition: Option<AuditionPresentation>,
    walk: crate::walk_view::WalkUi,
    /// Offscreen design renders draw walk controls as they look with live
    /// output. The app never sets this.
    walk_preview: bool,
}

#[derive(Clone, Debug)]
struct AuditionPresentation {
    mode: String,
    title: String,
    place_label: String,
    place_nonclaim: String,
    macro_ranges_m: [u32; 3],
}

impl From<&crate::fixture::FixtureAudition> for AuditionPresentation {
    fn from(value: &crate::fixture::FixtureAudition) -> Self {
        Self {
            mode: value.mode.clone(),
            title: value.title.clone(),
            place_label: value.place_label.clone(),
            place_nonclaim: value.place_nonclaim.clone(),
            macro_ranges_m: value.macro_ranges_m,
        }
    }
}

struct SourceView {
    id: String,
    asset_id: String,
    program_rms_dbfs: f32,
    song_fallback: bool,
    stereo_program: bool,
    onset_frames: usize,
    audition_label: String,
    macro_range_m: Option<u32>,
    position: EnuVector3,
    declared_spl_at_one_meter_db: f32,
    /// Rendered as `{spl_label}` every repaint; the declared SPL is fixed at
    /// scene load, so the label is formatted once instead of per frame.
    spl_label: String,
    monitor_offset_db: f32,
    retrigger_generation: u64,
    /// Impact start delay in frames, valid only for the named generation.
    retrigger_start_delay: Option<(u64, u32)>,
    enabled: bool,
    muted: bool,
    soloed: bool,
    street_height_m: f32,
    height: SourceHeight,
    trajectory: Option<SourceTrajectory>,
    acoustic: SourceAcousticState,
    occlusion_mode: DirectOcclusionMode,
    /// Rendered in the badge row every repaint; `occlusion_mode` is fixed at
    /// scene load, so its label is formatted once instead of per frame.
    occlusion_label: String,
    anomaly_descriptor: MultiSourceDescriptor,
    anomaly_asset_identity: String,
    /// Badge strings cached against the current `acoustic` state; rebuilt
    /// only when that state changes (see `BadgeTextCache`).
    badge_text: BadgeTextCache,
    /// A music source's own band envelope and kicks, read once at load for
    /// the City Map's music look (never from the audio callback).
    band_track: Option<std::sync::Arc<crate::map_link::BandTrack>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceComparisonMode {
    Raw,
    Spatial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceComparison {
    source_index: usize,
    mode: SourceComparisonMode,
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

    fn from_fixture(fixture: &Fixture) -> Self {
        let mut mix = Self::ALL_ENABLED;
        mix.bypassed[2] = !fixture.simulation.reflections.enabled;
        mix
    }

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
    retrigger_generations: [u64; fightbox_runtime::MAX_ACTIVE_SOURCES],
    /// Silence before a non-looping program restarts for its generation.
    retrigger_delay_frames: [u32; fightbox_runtime::MAX_ACTIVE_SOURCES],
}

impl SourceMix {
    const ALL_AUDIBLE: Self = Self {
        enabled: [true; fightbox_runtime::MAX_ACTIVE_SOURCES],
        muted: [false; fightbox_runtime::MAX_ACTIVE_SOURCES],
        soloed: [false; fightbox_runtime::MAX_ACTIVE_SOURCES],
        monitor_gains: [1.0; fightbox_runtime::MAX_ACTIVE_SOURCES],
        retrigger_generations: [0; fightbox_runtime::MAX_ACTIVE_SOURCES],
        retrigger_delay_frames: [0; fightbox_runtime::MAX_ACTIVE_SOURCES],
    };

    fn from_sources(sources: &[SourceView]) -> Self {
        let mut mix = Self::ALL_AUDIBLE;
        for (index, source) in sources.iter().enumerate() {
            mix.enabled[index] = source.enabled;
            mix.retrigger_generations[index] = source.retrigger_generation;
            mix.retrigger_delay_frames[index] = match source.retrigger_start_delay {
                Some((generation, frames)) if generation == source.retrigger_generation => frames,
                _ => 0,
            };
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

fn startup_source_enabled(
    fixture_default_enabled: bool,
    restart_on_enable: bool,
    saved_enabled: Option<bool>,
) -> bool {
    if restart_on_enable && !fixture_default_enabled {
        // Audition programs declared default-off remain off on every appearance,
        // even when an earlier sidecar saved them enabled.
        false
    } else {
        saved_enabled.unwrap_or(fixture_default_enabled)
    }
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
    Live(crate::live_input::LiveAudio),
    Stopped,
    Unavailable(String),
}

/// Consumes only a ready, unexpired interactive audition. A rebuild may supply a
/// new closure, but the shared reader still owns the original remaining budget.
fn take_quiet_start(
    pending: &mut Option<Box<dyn FnOnce() -> AudioState>>,
    reader: Option<&QuietOutputReader>,
) -> Option<Box<dyn FnOnce() -> AudioState>> {
    if reader.is_some_and(|reader| !reader.read().expired) {
        pending.take()
    } else {
        None
    }
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
        quiet_guard: Option<QuietOutputGuard>,
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
        let mut simulation_config = fixture.simulation_config();
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
                max_active_sources: fixture.runtime_source_count() as u8,
            },
        };

        let planned_source_ids = planned_physical_source_ids(fixture);
        let runtime_source_count = fixture.runtime_source_count();
        if runtime_source_count > fightbox_runtime::MAX_ACTIVE_SOURCES {
            return Err(format!(
                "scene {} requires {runtime_source_count} physical source slots",
                scene_spec.id
            ));
        }
        let mut prepared_sources = Vec::with_capacity(runtime_source_count);
        let mut echo_sources = Vec::with_capacity(runtime_source_count);
        let mut descriptors = Vec::with_capacity(runtime_source_count);
        let mut source_motion = [SourceMotion::default(); fightbox_runtime::MAX_ACTIVE_SOURCES];
        let mut source_views = Vec::with_capacity(runtime_source_count);
        let mut song_writers = Vec::with_capacity(fixture.sources.len());
        let mut song_readers = Vec::with_capacity(fixture.sources.len());
        let mut asset_loops = Vec::with_capacity(fixture.sources.len());
        for source in &fixture.sources {
            let index = prepared_sources.len();
            let position = source.initial_position()?;
            let trajectory = source
                .trajectory
                .as_ref()
                .map(SourceTrajectory::from_fixture)
                .transpose()?;
            let stereo_program = matches!(source.extent, fightbox_api::ExtentDescriptor::StereoImage { .. });
            let mut asset = if let Some(path) = source.song_file() {
                let (key, _) = song_asset_key(&scene_spec.path, path, source.extent);
                assets.get(&key).ok_or("declared song is missing from prepared cache")?.clone()
            } else if source.live_input.is_some() {
                PreparedAsset::live_music()
            } else {
                assets
                    .get(&source.asset_id)
                    .ok_or_else(|| format!("asset cache is missing {}", source.asset_id))?
                    .clone()
            };
            if !stereo_program {
                crate::asset::fold_song_to_mono(&mut asset)?;
            }
            let song_path = asset.song_path.clone();
            let program_rms_dbfs = asset.analysis.program_rms_dbfs;
            asset_loops.push(asset.loops);
            let (mut song_writer, song_reader) = song_channel();
            if song_path.is_some() || asset.stereo_samples.is_some() {
                song_writer.publish(SongBuffer { loaded_generation: 0, mono: asset.samples.clone(), stereo: asset.stereo_samples.take() });
            }
            song_writers.push(song_writer);
            song_readers.push(song_reader);
            let echo_impulse_class = if source.asset_id == "squad-a10-impacts" {
                fightbox_api::ImpulseClass::ArtilleryThunder
            } else {
                fightbox_api::ImpulseClass::None
            };
            let echo_profile = asset.echo_profile(source.impulsive, echo_impulse_class)?;
            echo_sources.push(echo_profile.is_enabled().then_some(position));
            let pose = Pose {
                position,
                forward: source.forward_enu_normalized()?,
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
            let descriptor = MultiSourceDescriptor::at(profile.pose.position)
                .with_reference_level(profile.reference_level)
                .with_directivity(profile.directivity)
                .with_extent(profile.extent)
                .with_echo_profile(echo_profile);
            descriptors.push(descriptor);
            let onset_frames = if asset.loops {
                0
            } else {
                crate::ballistic_crack::leading_onset_frames(&asset.samples)
            };
            let is_music = source.live_input.is_none()
                && (song_path.is_some()
                    || source.asset_id.contains("music")
                    || source.asset_id == "toms-diner"
                    || source.audition_label.as_deref().is_some_and(|label| {
                        let label = label.to_lowercase();
                        label.contains("music") || label.contains("speaker")
                    }));
            let band_track = (is_music && asset.samples.len() > SAMPLE_RATE as usize).then(|| {
                std::sync::Arc::new(crate::map_link::band_track(&asset.samples, SAMPLE_RATE))
            });
            prepared_sources.push((profile, asset.samples));
            source_motion[index] = SourceMotion {
                active: true,
                pose,
                linear_velocity_mps: EnuVector3::default(),
            };
            source_views.push(SourceView {
                id: source.id.clone(),
                asset_id: song_path
                    .as_ref()
                    .map(|path| format!("song:{}", path.display()))
                    .or_else(|| source.live_input.as_ref().map(|input| format!("live-input:{}", input.device)))
                    .unwrap_or_else(|| source.asset_id.clone()),
                program_rms_dbfs,
                song_fallback: source.live_input.is_some() || song_path.is_some()
                    || source.asset_id.contains("music") || source.asset_id == "toms-diner"
                    || source.audition_label.as_deref().is_some_and(|label| { let label = label.to_lowercase(); label.contains("music") || label.contains("speaker") }),
                stereo_program,
                onset_frames,
                audition_label: song_path
                    .as_ref()
                    .filter(|_| source.live_input.is_none())
                    .map(|path| song_file_label(path))
                    .or_else(|| source.audition_label.clone())
                    .unwrap_or_else(|| source.id.clone()),
                macro_range_m: source.macro_range_m,
                position,
                declared_spl_at_one_meter_db: source.reference_level.db_spl as f32,
                spl_label: format!(
                    "{} dB SPL",
                    format_db_number(source.reference_level.db_spl as f32)
                ),
                monitor_offset_db: source.monitor_offset_db,
                retrigger_generation: 0,
                retrigger_start_delay: None,
                enabled: source.default_enabled && song_path.is_none(),
                muted: false,
                soloed: false,
                street_height_m: position.up_m,
                height: SourceHeight::Street,
                trajectory,
                acoustic: SourceAcousticState::UNKNOWN,
                occlusion_mode: occlusion_mode_for_extent(simulation_config, source.extent),
                band_track,
                occlusion_label: occlusion_mode_text(occlusion_mode_for_extent(
                    simulation_config,
                    source.extent,
                )),
                anomaly_descriptor: descriptor,
                anomaly_asset_identity: format!("{}:{}", source.asset_id, asset.descriptor_sha256),
                badge_text: BadgeTextCache::default(),
            });
        }
        let mut ballistic_cracks = Vec::new();
        let mut crack_playback = Vec::new();
        for (parent_index, source) in fixture.sources.iter().enumerate() {
            if source.ballistic.is_none() && source.gunfire.is_none() {
                continue;
            }
            let slot_index = prepared_sources.len();
            let declaration = if let Some(gun) = &source.gunfire {
                if !asset_loops[parent_index] {
                    return Err(format!("source {} gunfire requires a looping asset", source.id));
                }
                BallisticCrack::declare_gun(
                    parent_index, slot_index, source, gun,
                    &prepared_sources[parent_index].1, SAMPLE_RATE, BLOCK_SIZE,
                    initial_listener.pose.position,
                )?
            } else {
                BallisticCrack::declare(
                    parent_index,
                    slot_index,
                    source,
                    source.ballistic.as_ref().unwrap(),
                    &prepared_sources[parent_index].1,
                    SAMPLE_RATE,
                    BLOCK_SIZE,
                    initial_listener.pose.position,
                )?
            };
            descriptors.push(declaration.descriptor);
            // CrackPlayback owns the prepared one-shot or loop stem; this
            // placeholder never supplies audio to a crack slot.
            prepared_sources.push((declaration.profile, Vec::new()));
            source_motion[slot_index] = SourceMotion {
                active: false,
                pose: declaration.pose,
                linear_velocity_mps: EnuVector3::default(),
            };
            ballistic_cracks.push(declaration.crack);
            crack_playback.push(declaration.playback);
        }
        // User mix defaults are resolved only after calibrated profiles and
        // backend descriptors are complete. They never alter the fixture's
        // calibrated source declarations; saved heights are applied to runtime
        // positions through the same control path as a UI selection below.
        let mut monitor_gain_db = if fixture.audition.is_some() {
            0.0
        } else {
            OutputSafetyConfig::DEFAULT_MONITOR_GAIN_DB
        };
        let mut mix_defaults_status = None;
        let mut saved_source_heights = Vec::new();
        match if args.replay.is_some() {
            Ok(None)
        } else {
            MixDefaults::read(&scene_spec.path)
        } {
            Ok(Some(defaults)) => {
                let valid_source_ids = fixture.sources.iter().map(|source| source.id.clone());
                let resolved = defaults.resolve(valid_source_ids);
                if fixture.audition.is_none() {
                    monitor_gain_db = resolved.monitor_gain_db;
                }
                for (index, source) in source_views.iter_mut().enumerate() {
                    if let Some(saved) = resolved.sources.get(&source.id) {
                        let fixture_source = &fixture.sources[index];
                        source.enabled = startup_source_enabled(
                            fixture_source.default_enabled && !source.asset_id.starts_with("song:"),
                            fixture_source.restart_on_enable || fixture_source.live_input.is_some() || source.asset_id.starts_with("song:"),
                            Some(saved.enabled),
                        );
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
        // Opt-in quiet start (the City Map launcher): the monitor gain starts
        // no louder than this; the slider still reaches the normal range.
        if args.replay.is_none() && fixture.audition.is_none() {
            if let Some(cap) = std::env::var("FIGHTBOX_START_GAIN_DB")
                .ok()
                .and_then(|value| value.trim().parse::<f32>().ok())
                .filter(|value| value.is_finite())
            {
                let cap = cap.clamp(MIN_MONITOR_GAIN_DB, MAX_MONITOR_GAIN_DB);
                if cap < monitor_gain_db {
                    eprintln!("[startup] quiet start: monitor gain {monitor_gain_db} -> {cap} dB");
                    monitor_gain_db = cap;
                }
            }
        }
        if let Some(replay) = &args.replay {
            monitor_gain_db = replay.monitor_gain_db as f32;
            for source in &mut source_views {
                source.enabled = fixture.cues.is_empty();
                source.muted = false;
                source.soloed = false;
            }
        }
        if !fixture.cues.is_empty() {
            for source in &mut source_views {
                source.enabled = false;
                source.soloed = false;
            }
        }
        tune_reflection_workers(&mut simulation_config, fixture.sources.len());
        let audio_config = AudioConfig {
            sample_rate_hz: SAMPLE_RATE as i32,
            frame_size: BLOCK_SIZE as i32,
        };
        let phase_started = Instant::now();
        let (mut runner, mut backend) = build_multi_source_session(
            scene_mesh,
            baked,
            audio_config,
            simulation_config,
            &descriptors,
        )
        .map_err(|error| format!("cannot build Steam Audio session: {error}"))?;
        if args.replay.as_ref().is_some_and(|r| r.pin_full_quality) {
            runner.pin_replay_full_quality();
        }
        let scene_air_control = runner.take_scene_air_control()
            .ok_or("Steam Audio simulation did not expose scene-air control")?;
        let mut stage_output_gain_control = backend
            .take_stage_output_gain_control()
            .ok_or("Steam Audio render graph did not expose stage-gain control")?;
        let scene_reset = (!fixture.cues.is_empty()).then(|| backend.scene_reset_control()).flatten();
        let echo_trigger_control = backend.take_echo_trigger_control();
        // Honor authored output bypass before any callback can render. Keep simulation
        // running so the existing live stage toggle can restore warm reflections.
        let stage_mix = StageMix::from_fixture(fixture);
        stage_output_gain_control
            .publish(stage_mix.gains())
            .map_err(|error| format!("cannot initialize authored stage gains: {error:?}"))?;
        let live_stage_energy = backend
            .take_live_stage_energy_reader()
            .ok_or("Steam Audio render graph did not expose live stage-energy telemetry")?;
        eprintln!(
            "[startup] steam scene + simulator build: {} ms",
            phase_started.elapsed().as_millis()
        );
        let probe_spheres = match baked.probe_coverage() {
            Ok(coverage) => Some(coverage.spheres().collect::<Vec<_>>()),
            Err(error) => {
                eprintln!("[startup] probe-coverage badges unavailable: {error}");
                None
            }
        };
        // Probe centres for the City Map's quality dots.
        let probe_points = probe_spheres
            .iter()
            .flatten()
            .map(|(center, _)| [center.x, center.y, center.z])
            .collect::<Vec<_>>();
        let probe_coverage = probe_spheres
            .map_or_else(ProbeCoverageQuery::unavailable, ProbeCoverageQuery::from_spheres);
        let (callback_timing_writer, callback_timing_reader) = CallbackTimingPublication::new();
        let initial_update = SimulationUpdate {
            listener: initial_listener,
            sources: source_motion,
        };
        let reflection_warmup_started = Instant::now();
        if fixture.sources.len() > 1 {
            runner.prepare_simulation_for_realtime(&initial_update)
                .map_err(|error| format!("cannot prepare initial acoustics: {error:?}"))?;
            runner.enable_reflection_worker(
                1_000_000_000 / u64::from(SimulationCadences::default().reflection_max_hz),
            )
            .map_err(|error| format!("cannot start reflection worker: {error:?}"))?;
        }
        let (runner, acoustic_telemetry) =
            AcousticTelemetryTap::new(runner, callback_timing_reader);
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
        let mut program_plane_counts = fixture
            .sources
            .iter()
            .map(|source| if matches!(source.extent, fightbox_api::ExtentDescriptor::StereoImage { .. }) { 2 } else { 1 })
            .collect::<Vec<_>>();
        program_plane_counts.resize(prepared_sources.len(), 1);
        let ambix_export = args.render_format == RenderFormat::Ambix;
        let mut graph = if ambix_export {
            RuntimeGraph::new_with_spatial_backend_and_output_safety(
                engine_config, reader, output_safety_reader, &program_plane_counts,
                backend.into_spatial_export().map_err(|error| format!("cannot prepare AmbiX export: {error}"))?,
            )
        } else if program_plane_counts.contains(&2) {
            RuntimeGraph::new_with_program_backend_and_output_safety(
                engine_config,
                reader,
                output_safety_reader,
                &program_plane_counts,
                Box::new(backend),
            )
        } else {
            RuntimeGraph::new_with_backend_and_output_safety(
                engine_config,
                reader,
                output_safety_reader,
                Box::new(backend),
            )
        }
        .map_err(|error| format!("cannot create runtime graph: {error:?}"))?;
        let (monitor_route_controller, monitor_route_reader) = MonitorRoutePublication::new();
        graph.set_monitor_route_reader(monitor_route_reader);
        graph.set_listener_state(initial_listener);
        for (index, (profile, _)) in prepared_sources.iter().enumerate() {
            graph
                .set_source(index, profile, SceneCalibration::default())
                .map_err(|error| format!("cannot configure source {index}: {error:?}"))?;
        }
        if ambix_export {
            graph.prepare_spatial_backend_for_realtime()
                .map_err(|error| format!("cannot prepare spatial render graph: {error:?}"))?;
        } else if fixture.sources.len() > 1 {
            let silent = [0.0; BLOCK_SIZE as usize];
            let sources = program_plane_counts.iter().enumerate().map(|(index, planes)| {
                fightbox_runtime::backend::SpatialProgramBlock {
                    source_index: index, program_plane_count: *planes,
                    program_planes: [&silent, if *planes == 2 { &silent } else { &[] }],
                }
            }).collect::<Vec<_>>();
            let mut left = silent;
            let mut right = silent;
            // Warm retained SDK FFT/HRTF state on control, before any callback.
            for _ in 0..16 {
                graph.process_program_block(fightbox_runtime::ProgramProcessBlock {
                    now_ns: 0, sources: &sources,
                    output_left: &mut left, output_right: &mut right,
                }).map_err(|error| format!("cannot warm render graph: {error:?}"))?;
            }
        }
        eprintln!(
            "[startup] runtime graph configuration: {} ms",
            phase_started.elapsed().as_millis()
        );
        let (meter_writer, meter_reader) = SnapshotPublication::new(MeterReading::SILENT);
        let initial_source_mix = SourceMix::from_sources(&source_views);
        let (source_mix_writer, source_mix_reader) = SnapshotPublication::new(initial_source_mix);
        let scene_control = SceneControl { listener: listener.position, ..SceneControl::default() };
        let (scene_control_writer, scene_control_reader) = SnapshotPublication::new(scene_control);
        let scene_timeline = (!fixture.cues.is_empty()).then(|| SceneTimeline::new(fixture, SAMPLE_RATE));
        let (playback_status_writer, playback_status_reader) =
            SnapshotPublication::new(PlaybackSnapshot::default());
        let (trace_playback_writer, trace_playback_reader) =
            SnapshotPublication::new(PlaybackSnapshot::default());
        let (trace_control_writer, trace_control_reader) =
            SnapshotPublication::new(TraceControl::default());
        let (trace_writer, trace_reader) = crate::level_trace::channel(SAMPLE_RATE, 256, 6000)
            .map_err(|error| format!("cannot create measured level trail: {error}"))?;
        let (audio_block_writer, audio_block_reader) = SnapshotPublication::new(0_u64);
        let capture_root = match &args.replay {
            Some(replay) => replay.capture_root.clone(),
            None => default_capture_root()?,
        };
        let browser = scan_capture_bundles(&capture_root).unwrap_or_else(|error| BrowserScan {
            entries: vec![],
            warnings: vec![error],
        });
        let (capture, capture_tap) = CaptureController::new(capture_root.clone());
        let ambix_capture = ambix_export.then(|| crate::spatial_export::StemCapture::new(
            args.replay.as_ref().expect("AmbiX export is headless").seconds as usize * SAMPLE_RATE as usize,
        ));
        let graph = match &ambix_capture {
            Some(capture) => SceneRenderer::Ambix(crate::spatial_export::SpatialExportGraph::new(
                graph, initial_listener, std::sync::Arc::clone(capture),
            )),
            None => SceneRenderer::Binaural(graph),
        };
        let quiet_output = quiet_guard.as_ref().map(QuietOutputGuard::reader);
        let mut processor = LateBoundProcessor::new(
            graph,
            pose_reader,
            meter_writer,
            MeterAccumulator::new(SAMPLE_RATE, BLOCK_SIZE, METER_WINDOW_SECONDS),
            audio_block_writer,
            Some(capture_tap),
            quiet_guard,
        );
        if !fixture.cues.is_empty() {
            // Scenes start silent; the gate fades in at Play.
            processor.scene_gate = true;
            processor.gate_gain = 0.0;
        }
        processor.level_trace = Some(WorkbenchTraceTap {
            writer: trace_writer,
            control_reader: trace_control_reader,
            playback_reader: trace_playback_reader,
            last_mix: initial_source_mix,
            last_config_epoch: 0,
            epoch: 0,
        });
        let playback: Vec<SourcePlayback> = source_views
            .iter()
            .zip(&fixture.sources)
            .zip(&prepared_sources)
            .zip(asset_loops)
            .map(|(((source, fixture_source), (_, samples)), loops)| {
                let mut playback = SourcePlayback::for_asset(
                    &source.asset_id,
                    SAMPLE_RATE,
                    fixture_source.playback_start_offset_s,
                    samples.len(),
                    fixture_source.restart_on_enable || source.asset_id.starts_with("song:"),
                    loops,
                );
                playback.honor_loop_delay = fixture_source.gunfire.is_some();
                playback
            })
            .collect();
        // One-shot echo sources freeze a plan on each shot; looping ones keep
        // their descriptor onset clock.
        let triggered = echo_sources
            .iter()
            .zip(&playback)
            .map(|(source, playback)| source.filter(|_| playback.is_one_shot() || !fixture.cues.is_empty()))
            .collect::<Vec<_>>();
        let (host_echoes, shot_trigger) = match echo_trigger_control {
            Some(control) if triggered.iter().any(Option::is_some) => {
                let (echoes, trigger) = crate::echo_paths::HostEchoes::start(
                    control,
                    &package.mesh,
                    &package.materials,
                    &triggered,
                    listener.position,
                    fixture.air_exponents(),
                )?;
                (Some(echoes), Some(trigger))
            }
            _ => (None, None),
        };
        let signals = prepared_sources
            .into_iter()
            .take(source_views.len())
            .map(|(_, samples)| samples)
            .collect();
        let mut live_devices = fixture
            .sources
            .iter()
            .map(|source| source.live_input.as_ref().map(|input| input.device.clone()))
            .collect::<Vec<_>>();
        if args.program_file.is_some() {
            let index = song_target(&source_views, None).ok_or("scene has no song source")?;
            live_devices[index] = None;
        }
        let live_mono = fixture.sources.iter().map(|source|
            source.live_input.as_ref().is_some_and(|input| input.channels.plane_count() == 1)
        ).collect::<Vec<_>>();
        let live_input_wav = args.live_input_wav.clone();
        let null_output = args.null_output;
        let null_frame_limit = args
            .replay
            .as_ref()
            .map(|replay| u64::from(replay.seconds) * u64::from(SAMPLE_RATE));
        let phase_started = Instant::now();
        let mut pending_audio: Option<Box<dyn FnOnce() -> AudioState>> = None;
        let audio = if args.replay.is_some() || args.quiet_audition.is_some() {
            let device = args.device.clone();
            if !null_output && device.is_none() {
                return Err("deferred audio requires an explicit device".into());
            }
            pending_audio = Some(Box::new(move || {
                start_audio(
                    processor,
                    engine_config,
                    signals,
                    live_devices,
                    live_mono,
                    program_plane_counts,
                    song_readers,
                    live_input_wav,
                    playback,
                    crack_playback,
                    scene_timeline,
                    scene_control_reader,
                    scene_reset,
                    source_mix_reader,
                    playback_status_writer,
                    trace_playback_writer,
                    callback_timing_writer,
                    shot_trigger,
                    device.as_deref(),
                    true,
                    null_output,
                    null_frame_limit,
                )
            }));
            AudioState::Stopped
        } else if args.start_audio {
            let audio = start_audio(
                processor,
                engine_config,
                signals,
                live_devices,
                live_mono,
                program_plane_counts,
                song_readers,
                live_input_wav,
                playback,
                crack_playback,
                scene_timeline,
                scene_control_reader,
                scene_reset,
                source_mix_reader,
                playback_status_writer,
                trace_playback_writer,
                callback_timing_writer,
                shot_trigger,
                args.device.as_deref(),
                args.quiet_audition.is_some(),
                null_output,
                None,
            );
            eprintln!(
                "[startup] explicitly requested audio stream initialization: {} ms",
                phase_started.elapsed().as_millis()
            );
            audio
        } else {
            drop((
                processor,
                signals,
                playback,
                crack_playback,
                source_mix_reader,
                playback_status_writer,
                trace_playback_writer,
                callback_timing_writer,
            ));
            eprintln!("[startup] audio stream not opened: explicit --start-audio was absent");
            AudioState::Stopped
        };
        let phase_started = Instant::now();
        let faces = mesh_faces(&package.mesh);
        let face_colors = faces
            .iter()
            .map(|face| face_color(*face))
            .collect::<Vec<_>>();
        let camera = Camera::for_mesh(&package.mesh);
        let feed_channel = SnapshotPublication::new(None);
        let scene_bounds = Bounds2::for_mesh(&package.mesh);
        let ground_map = build_ground_map(
            &package.mesh,
            scene_bounds,
            fixture.street_lines_m.clone(),
        );
        let autopilot =
            Autopilot::for_scene(scene_bounds, fixture, &scene_spec.id, listener.position);
        let source_height_levels = SourceHeightLevels::for_mesh(&package.mesh);
        let anomaly_field = FieldController::new(FieldContext::new(
            package.clone(),
            scene_mesh.clone(),
            args.baked.clone(),
            simulation_config,
            [scene_bounds.min, scene_bounds.max],
            capture_static.fixture_content_sha256.clone(),
            baked.metadata.content_sha256.clone(),
            format!(
                "{:?}:dirty={:?}",
                capture_static.engine_commit, capture_static.engine_dirty
            ),
            capture_root.join("anomaly-fields"),
        ));
        eprintln!(
            "[startup] workbench view preparation: {} ms",
            phase_started.elapsed().as_millis()
        );
        let mut feed_planner = crate::echo_paths::EchoPathPlanner::new(
            &package.mesh, &package.materials,
        )?;
        feed_planner.set_air_exponents(fixture.air_exponents());
        let speaker_songs = source_views
            .iter()
            .map(|source| source.asset_id.strip_prefix("song:").map(PathBuf::from))
            .collect();
        let mut workbench = Self {
            mesh: package.mesh.clone(),
            faces,
            face_colors,
            sources: source_views,
            mix_gains_cache: None,
            overlay_identity_cache: None,
            listener,
            head_tracking: HeadTracking::default(),
            pose_mailbox,
            simulation,
            source_motion,
            ballistic_cracks,
            host_echoes,
            feed_planner,
            feed_writer: feed_channel.0,
            feed_reader: feed_channel.1,
            feed_events: Vec::new(),
            feed_history: Vec::new(),
            feed_sequences: [0; fightbox_runtime::MAX_ACTIVE_SOURCES],
            feed_audio_sample: 0,
            feed_contexts: vec![None; fixture.sources.len()],
            audio,
            pending_audio,
            song_writers,
            song_load: None,
            song_status: None,
            music_speaker: fixture.sources.iter().position(|source| source.live_input.is_some()),
            speaker_songs,
            drop_markers: Vec::new(),
            quiet_output,
            camera,
            monitor_gain_db,
            scene_air: fixture.air.unwrap_or(FixtureAir::Preset(AirPreset::Temperate)),
            scene_air_exponents: fixture.air_exponents(),
            scene_air_control,
            output_safety_controller,
            meter_reader,
            source_mix_writer,
            scene_control,
            scene_control_writer,
            scene_cues: fixture.cues.clone(),
            scene_prepared_listener: None,
            playback_status_reader,
            level_trace: crate::level_trace_ui::LevelTraceUi::new(trace_reader),
            trace_control_writer,
            trace_generation: 0,
            trace_config_epoch: 0,
            trace_last_config: None,
            trace_recording_snapshots: VecDeque::new(),
            trace_export_status: None,
            monitor_route_controller,
            source_comparison: None,
            stage_mix,
            stage_output_gain_control,
            audio_block_reader,
            capture,
            ambix_capture,
            capture_state: CaptureUiState::Idle,
            capture_static,
            capture_entries: browser.entries,
            capture_warnings: browser.warnings,
            capture_status: None,
            fixture_path: scene_spec.path.clone(),
            scene_positions: crate::scene_positions::ScenePositions::read(&scene_spec.path)?,
            scene_save_status: None,
            saved_fixture: None,
            source_drag: None,
            mix_defaults_status,
            autopilot,
            source_height_levels,
            probe_coverage,
            probe_points,
            map: MapLinkState::default(),
            acoustic_telemetry,
            live_stage_energy,
            anomaly_field,
            listening_mode: !fixture.cues.is_empty()
                || scene_spec.id == "astra-artillery-street-path-candidate-v1"
                || fixture
                    .sources
                    .iter()
                    .any(|source| source.live_input.is_some()),
            ground_map_enabled: false,
            ground_map_whole_scene: false,
            ground_map_local_frame: crate::ground_map::LocalSoundFrame::default(),
            ground_map,
            visibility_range,
            startup_started,
            reflection_warmup_started,
            reflection_warmup_reported: false,
            first_frame_reported: false,
            output_device_label: args.device.clone().unwrap_or_else(|| {
                if args.null_output {
                    "null-output"
                } else {
                    "system default output"
                }
                .to_owned()
            }),
            audition: fixture.audition.as_ref().map(AuditionPresentation::from),
            walk: crate::walk_view::WalkUi {
                design: crate::walk_view::WalkDesign::from_env(),
                atlas: crate::walk_view::StreetAtlas::new(
                    &fixture.street_lines_m,
                    &fixture.street_names,
                    &fixture.street_kinds,
                ),
                presets: Vec::new(),
                authored_width_m: Vec::new(),
                ground_up_m: package
                    .mesh
                    .vertices_enu_m
                    .iter()
                    .map(|vertex| vertex.up_m)
                    .reduce(f32::min)
                    .unwrap_or_default(),
                placing: None,
            },
            walk_preview: false,
        };
        for (index, height) in saved_source_heights {
            workbench.apply_source_height(index, height);
        }
        workbench.walk.authored_width_m = workbench
            .sources
            .iter()
            .map(|source| {
                match fixture.sources.iter().find(|fixture| fixture.id == source.id).map(|fixture| fixture.extent) {
                    Some(fightbox_api::ExtentDescriptor::LineSegment { length_m }) => length_m,
                    Some(fightbox_api::ExtentDescriptor::StereoImage { width_m }) => width_m,
                    _ => 0.0,
                }
            })
            .collect();
        workbench.walk.presets = workbench
            .sources
            .iter()
            .map(|source| {
                source
                    .trajectory
                    .is_none()
                    .then(|| crate::walk_view::matching_preset(source.declared_spl_at_one_meter_db))
                    .flatten()
            })
            .collect();
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

    fn quiet_ready(&self) -> bool {
        self.pending_audio.is_some()
            && self
                .quiet_output
                .as_ref()
                .is_some_and(|reader| !reader.read().expired)
    }

    fn start_quiet_audition(&mut self) {
        if let Some(start) = take_quiet_start(&mut self.pending_audio, self.quiet_output.as_ref()) {
            // Source setup has been published by the controls before this button.
            self.audio = start();
        }
    }

    fn stop_audio(&self) -> Result<(), String> {
        match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(output) => output
                .stop()
                .map_err(|error| format!("cannot pause output for scene rebuild: {error:?}")),
            AudioState::Stopped | AudioState::Unavailable(_) => Ok(()),
        }
    }

    fn update_source_motion(&mut self) {
        let elapsed_blocks = self.audio_block_reader.read();
        self.update_source_motion_at_block(elapsed_blocks);
    }

    /// Shares the live source update route with headless replay at one explicit clock sample.
    fn update_source_motion_at_block(&mut self, elapsed_blocks: u64) {
        let playback = self.playback_status_reader.read();
        for index in 0..self.sources.len() {
            let status = playback.sources[index];
            let scene_frames = if self.scene_cues.is_empty() {
                elapsed_blocks * u64::from(BLOCK_SIZE)
            } else if status.enabled {
                status.audio_sample.saturating_sub(status.trigger_audio_sample)
            } else {
                0
            };
            let Some((mut sample, speed_mps)) =
                self.sources[index].trajectory.as_ref().map(|trajectory| {
                    (
                        trajectory.sample_at_frame(scene_frames),
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
            self.source_motion[index].linear_velocity_mps = if !self.scene_cues.is_empty() && !status.enabled {
                EnuVector3::default()
            } else {
                scale(sample.direction, speed_mps)
            };
        }
    }

    /// Re-resolves every source's badge row from the latest simulation
    /// publication and the source's current position. Control-tick only.
    fn refresh_acoustic_state(&mut self) {
        let telemetry = self.acoustic_telemetry.read();
        let listener_probes = self.probe_coverage.coverage(self.listener.position);
        let stage_gains = self.stage_mix.gains();
        // The mix only changes at UI edit boundaries, which invalidate the
        // cache; between edits the gains array is reused as-is.
        if !self.scene_cues.is_empty() { self.mix_gains_cache = None; }
        if self.mix_gains_cache.is_none() {
            self.mix_gains_cache =
                Some(self.observed_mix().gains(self.sources.len()));
        }
        let mix_gains = self.mix_gains_cache.unwrap();
        for (index, source) in self.sources.iter_mut().enumerate() {
            let inputs = SourceAcousticInputs {
                source_probes: self.probe_coverage.coverage(source.position),
                listener_probes,
                audible_in_mix: mix_gains[index] > 0.0,
                stage_gains,
            };
            let updated = SourceAcousticState::evaluate(inputs, telemetry, index);
            source.badge_text.refresh(updated);
            source.acoustic = updated;
        }
    }

    fn update_source_position(&mut self, index: usize, position: EnuVector3) {
        let changed = self.sources[index].position != position;
        self.sources[index].position = position;
        self.source_motion[index].pose.position = position;
        self.output_safety_controller
            .set_source_position(index, position)
            .expect("workbench source positions remain finite");
        if changed && index == self.anomaly_field.selected_source {
            self.anomaly_field
                .invalidate("selected source pose changed");
        }
        if changed && self.source_drag.is_none() && let Some(echoes) = &mut self.host_echoes {
            echoes.move_source(index, position, self.listener.position);
        }
    }

    fn replan_placed_source(&mut self, index: usize) {
        let position = self.sources[index].position;
        if let Some(echoes) = &mut self.host_echoes {
            echoes.move_source(index, position, self.listener.position);
            if let Err(error) = echoes.publish(index, position, self.listener.position) {
                eprintln!("[echo] {error}");
            }
        }
        self.feed_contexts[index] = None;
        let Some(previous) = self
            .feed_events
            .iter()
            .find(|event| event.source_index == index)
            .copied()
        else {
            return;
        };
        let published = self
            .host_echoes
            .as_ref()
            .and_then(|echoes| echoes.published_plan(index));
        let fallback;
        let plan = match published {
            Some(plan) => plan,
            None => {
                let field = self.feed_planner.route_field(position);
                fallback = self.feed_planner.plan(&field, self.listener.position);
                &fallback
            }
        };
        match crate::acoustic_feed::AcousticEvent::from_plan(
            &self.sources[index].id,
            index,
            previous.event_sequence,
            SAMPLE_RATE,
            previous.trigger_audio_sample,
            position,
            self.listener.position,
            previous.source_emission_time_s,
            plan,
            published.is_some(),
            previous.crack,
        ) {
            Ok(event) => {
                self.feed_writer.publish(Some(event));
                self.feed_events.retain(|old| old.source_index != index);
                self.feed_events.push(event);
                if let Some(old) = self.feed_history.iter_mut().rev().find(|old| {
                    old.source_index == index && old.event_sequence == event.event_sequence
                }) {
                    *old = event;
                }
            }
            Err(error) => eprintln!("[seeing] {error}"),
        }
    }

    fn update_source_drag(&mut self, pointer: Pos2, release: bool) {
        let Some(drag) = &mut self.source_drag else {
            return;
        };
        let index = drag.index;
        let position = drag.update(pointer, &self.probe_coverage, |point| {
            crate::source_drag::ground_height(&self.mesh, point)
        });
        let replan =
            drag.planned != position && (release || drag.last_plan.elapsed().as_millis() >= 100);
        if replan {
            drag.planned = position;
            drag.last_plan = Instant::now();
        }
        self.update_source_position(index, position);
        self.simulation.publish_update(SimulationUpdate {
            listener: self.listener.listener_state(EnuVector3::default()),
            sources: self.source_motion,
        });
        if replan {
            self.replan_placed_source(index);
        }
        if release {
            self.source_drag = None;
            self.sources[index].street_height_m = position.up_m;
            self.sources[index].height = SourceHeight::Street;
            let source = &self.sources[index];
            self.scene_save_status = self
                .scene_positions
                .set_position(
                    &source.id,
                    [position.east_m, position.north_m, position.up_m],
                )
                .err();
        }
    }

    fn scene_save_controls(&mut self, ui: &mut egui::Ui) -> Rect {
        ui.horizontal(|ui| {
            let button = ui.add_enabled(
                self.scene_positions.is_dirty() && self.source_drag.is_none(),
                egui::Button::new("Save scene"),
            );
            if button.clicked() {
                self.scene_save_status = Some(match self.scene_positions.save() {
                    Ok(()) => {
                        self.saved_fixture = Fixture::read(&self.fixture_path).ok();
                        if let Some(hash) = sha256_file(&self.fixture_path) {
                            self.capture_static.fixture_content_sha256 = hash;
                        }
                        "Saved".into()
                    }
                    Err(error) => error,
                });
            }
            if let Some(status) = &self.scene_save_status {
                ui.small(status);
            }
            button.rect
        })
        .inner
    }

    fn apply_source_height(&mut self, index: usize, selection: SourceHeight) {
        self.sources[index].height = selection;
        let mut position = self.sources[index].position;
        position.up_m = self
            .source_height_levels
            .height_m(selection, self.sources[index].street_height_m);
        self.update_source_position(index, position);
    }

    fn anomaly_source_query(&self, index: usize) -> SourceQuery {
        let source = &self.sources[index];
        source_query(
            &source.id,
            source.position,
            source.declared_spl_at_one_meter_db,
            source.anomaly_descriptor,
            source.anomaly_asset_identity.clone(),
        )
    }

    fn select_source_comparison(&mut self, source_index: usize, mode: SourceComparisonMode) {
        for (index, source) in self.sources.iter_mut().enumerate() {
            source.soloed = index == source_index;
        }
        self.arm_source_play(source_index, mode);
        self.source_mix_writer
            .publish(SourceMix::from_sources(&self.sources));
        self.mix_gains_cache = None;
        match mode {
            SourceComparisonMode::Raw => self
                .monitor_route_controller
                .select_raw_source(source_index)
                .expect("workbench sources always fit the runtime source capacity"),
            SourceComparisonMode::Spatial => {
                self.monitor_route_controller.select_spatial();
            }
        }
        self.source_comparison = Some(SourceComparison { source_index, mode });
    }

    fn arm_source_play(&mut self, source_index: usize, mode: SourceComparisonMode) {
        #[cfg(feature = "live-output")]
        if self.sources[source_index].asset_id.starts_with("live-input:")
            && let AudioState::Live(output) = &mut self.audio
            && let Err(error) = output.start_input(source_index)
        {
            eprintln!("[live input] {error}");
            self.sources[source_index].enabled = false;
            return;
        }
        let source = &mut self.sources[source_index];
        source.enabled = true;
        source.muted = false;
        source.retrigger_generation = source.retrigger_generation.wrapping_add(1);
        self.arm_ballistic_shot(source_index, mode);
        self.feed_contexts[source_index] = Some((
            self.sources[source_index].retrigger_generation,
            self.sources[source_index].position,
            self.listener.position,
        ));
        // Plan for where the listener stands now, before the shot restarts.
        if let Some(echoes) = &mut self.host_echoes
            && let Err(error) = echoes.publish(
                source_index,
                self.sources[source_index].position,
                self.listener.position,
            )
        {
            eprintln!("[echo] {error}");
        }
    }

    /// Render entry point: one Play at audio t=0, before output starts.
    fn play_fixture_at_start(&mut self) {
        for index in 0..self.sources.len() {
            self.sources[index].soloed = false;
            self.arm_source_play(index, SourceComparisonMode::Spatial);
        }
        self.source_mix_writer
            .publish(SourceMix::from_sources(&self.sources));
        self.mix_gains_cache = None;
        self.monitor_route_controller.select_spatial();
        self.source_comparison = None;
    }

    /// Plans a ballistic source's shot for the current listener before the
    /// mix publication that carries its retrigger. The crack stem is
    /// published first, so the audio thread finds it with that generation.
    fn arm_ballistic_shot(&mut self, source_index: usize, mode: SourceComparisonMode) {
        let generation = self.sources[source_index].retrigger_generation;
        self.sources[source_index].retrigger_start_delay = None;
        if mode != SourceComparisonMode::Spatial || self.sources[source_index].asset_id.starts_with("song:") {
            return;
        }
        let Some(crack) = self
            .ballistic_cracks
            .iter_mut()
            .find(|crack| crack.parent_index == source_index)
        else {
            return;
        };
        self.source_motion[crack.slot_index].active = false;
        crack.release_after_block = None;
        let shot = match crack.arm(generation, self.listener.position) {
            Ok(shot) => shot,
            Err(error) => {
                eprintln!("[ballistic] shot not armed, impact plays alone: {error}");
                return;
            }
        };
        self.sources[source_index].retrigger_start_delay = Some((generation, shot.impact_delay_frames));
        let Some(armed) = shot.crack else {
            return;
        };
        if let Err(error) =
            self.output_safety_controller
                .set_source(crack.slot_index, &armed.profile, None)
        {
            eprintln!("[ballistic] crack safety calibration rejected: {error:?}");
            return;
        }
        let motion = &mut self.source_motion[crack.slot_index];
        motion.pose.position = armed.profile.pose.position;
        motion.active = true;
        crack.release_after_block = (!crack.is_looping()).then(|| {
            self.audio_block_reader
                .read()
                .saturating_add(armed.release_blocks)
        });
    }

    /// Deactivates crack slots whose last delayed sample has left the engine,
    /// so the next shot is a fresh transient activation.
    fn release_ballistic_cracks(&mut self) {
        if self
            .ballistic_cracks
            .iter()
            .all(|crack| crack.release_after_block.is_none())
        {
            return;
        }
        let elapsed_blocks = self.audio_block_reader.read();
        for crack in &mut self.ballistic_cracks {
            if crack
                .release_after_block
                .is_some_and(|deadline| elapsed_blocks >= deadline)
            {
                crack.release_after_block = None;
                self.source_motion[crack.slot_index].active = false;
            }
        }
    }

    /// Turns one sound on (from its start) or off; the others keep playing.
    fn toggle_sound(&mut self, index: usize, on: bool) {
        for source in &mut self.sources {
            source.soloed = false;
        }
        if on {
            self.sources[index].enabled = false;
            #[cfg(feature = "live-output")]
            if let AudioState::Live(output) = &mut self.audio {
                output.stop_input(index);
            }
        } else {
            self.arm_source_play(index, SourceComparisonMode::Spatial);
        }
        self.source_mix_writer
            .publish(SourceMix::from_sources(&self.sources));
        self.mix_gains_cache = None;
        self.monitor_route_controller.select_spatial();
        self.source_comparison = None;
    }

    fn stop_demo_sources(&mut self) {
        if !self.scene_cues.is_empty() {
            self.scene_control.running = false;
            self.scene_control_writer.publish(self.scene_control);
            self.scene_prepared_listener = None;
            self.feed_events.clear();
            for crack in &mut self.ballistic_cracks {
                crack.release_after_block = None;
                self.source_motion[crack.slot_index].active = false;
            }
        }
        for source in &mut self.sources {
            source.enabled = false;
            source.soloed = false;
        }
        self.source_mix_writer
            .publish(SourceMix::from_sources(&self.sources));
        #[cfg(feature = "live-output")]
        if let AudioState::Live(output) = &mut self.audio { output.stop_inputs(); }
        self.mix_gains_cache = None;
        self.monitor_route_controller.select_spatial();
        self.source_comparison = None;
    }

    fn play_scene(&mut self) {
        self.stop_demo_sources();
        self.scene_control.generation = self.scene_control.generation.wrapping_add(1);
        self.scene_control.running = true;
        self.prepare_scene_sources();
        self.scene_control.listener = self.listener.position;
        self.scene_control_writer.publish(self.scene_control);
    }

    /// Plans on the control thread; the callback only consumes prepared stems,
    /// delays and echo tables. Replan when walking changes the listener spot.
    fn prepare_scene_sources(&mut self) {
        for index in 0..self.sources.len() {
            // A walking scene preparation updates the gun's current bank in
            // place below; changing its identity would restart the round clock.
            if self.scene_prepared_listener.is_some() && self.ballistic_cracks.iter()
                .any(|crack| crack.parent_index == index && crack.is_looping()) { continue; }
            // An out-of-cone preparation publishes no stem. A fresh identity
            // prevents a later cue from adopting an earlier in-cone bank.
            self.sources[index].retrigger_generation = self.sources[index].retrigger_generation.wrapping_add(1);
            self.arm_ballistic_shot(index, SourceComparisonMode::Spatial);
            if let Some(crack) = self.ballistic_cracks.iter_mut().find(|crack| crack.parent_index == index) {
                crack.release_after_block = None;
            }
            if let Some(echoes) = &mut self.host_echoes
                && let Err(error) = echoes.publish(index, self.sources[index].position, self.listener.position)
            {
                eprintln!("[echo] {error}");
            }
        }
        self.scene_prepared_listener = Some(self.listener.position);
        let mix = SourceMix::from_sources(&self.sources);
        self.scene_control.prepared_generations = mix.retrigger_generations;
        self.scene_control.delay_frames = mix.retrigger_delay_frames;
        self.source_mix_writer.publish(mix);
        self.mix_gains_cache = None;
    }

    fn observed_mix(&mut self) -> SourceMix {
        if self.scene_cues.is_empty() {
            SourceMix::from_sources(&self.sources)
        } else {
            self.playback_status_reader.read().consumed_mix
        }
    }

    fn refresh_acoustic_feed(&mut self) {
        use crate::acoustic_feed::AcousticEvent;
        let playback = self.playback_status_reader.read();
        self.feed_audio_sample = playback
            .sources
            .iter()
            .map(|source| source.audio_sample)
            .max()
            .unwrap_or(0);
        for index in 0..self.sources.len() {
            let status = playback.sources[index];
            if !status.enabled {
                self.feed_contexts[index] = None;
            }
            if status.event_sequence == self.feed_sequences[index] {
                continue;
            }
            self.feed_sequences[index] = status.event_sequence;
            if self.source_comparison.is_some_and(|comparison| {
                comparison.source_index == index && comparison.mode == SourceComparisonMode::Raw
            }) {
                continue;
            }
            let source = &self.sources[index];
            let (position, listener) = self.feed_contexts[index]
                .filter(|(generation, _, _)| *generation == status.generation)
                .map(|(_, source, listener)| (source, listener))
                .unwrap_or((source.position, self.listener.position));
            let published = self
                .host_echoes
                .as_ref()
                .and_then(|echoes| echoes.published_plan(index));
            let fallback;
            let plan = match published {
                Some(plan) => plan,
                None => {
                    let field = self.feed_planner.route_field(position);
                    fallback = self.feed_planner.plan(&field, listener);
                    &fallback
                }
            };
            let emission_s = (u64::from(status.event_delay_frames) + source.onset_frames as u64)
                as f64
                / f64::from(SAMPLE_RATE);
            let crack = (status.event_delay_frames > 0)
                .then(|| {
                    self.ballistic_cracks
                        .iter()
                        .find(|crack| crack.parent_index == index)
                        .and_then(|crack| crack.feed_crack(listener))
                })
                .flatten();
            match AcousticEvent::from_plan(
                &source.id,
                index,
                status.event_sequence,
                SAMPLE_RATE,
                status.trigger_audio_sample,
                position,
                listener,
                emission_s,
                plan,
                published.is_some(),
                crack,
            ) {
                Ok(event) => {
                    self.feed_writer.publish(Some(event));
                    if let Some(event) = self.feed_reader.read() {
                        self.feed_events.retain(|old| old.source_index != index);
                        self.feed_events.push(event);
                        // Retain the latest 256 events for bounded headless replays.
                        if self.feed_history.len() == 256 {
                            self.feed_history.remove(0);
                        }
                        self.feed_history.push(event);
                    }
                }
                Err(error) => eprintln!("[seeing] {error}"),
            }
        }
    }

    fn air_combo(&mut self, ui: &mut egui::Ui) {
        let previous = self.scene_air;
        let label = match self.scene_air {
            FixtureAir::Preset(preset) => preset.label(),
            FixtureAir::Observation(_) => "Custom",
        };
        egui::ComboBox::from_id_salt("scene_air")
            .selected_text(format!("Air: {label}"))
            .width(110.0)
            .show_ui(ui, |ui| {
                for preset in AirPreset::ALL {
                    ui.selectable_value(&mut self.scene_air, FixtureAir::Preset(preset), preset.label());
                }
            });
        if previous != self.scene_air {
            let exponents = fightbox_runtime::FrozenAtmosphere::freeze(Some(self.scene_air.observation()))
                .three_band_air_pressure_exponents_per_m();
            self.scene_air_control.publish(exponents).expect("validated scene air");
            self.scene_air_exponents = exponents;
            self.feed_planner.set_air_exponents(exponents);
            if let Some(echoes) = &mut self.host_echoes {
                echoes.set_air_exponents(exponents);
                // Only future triggers adopt these plans; sounding tails remain frozen.
                for (index, source) in self.sources.iter().enumerate() {
                    if let Err(error) = echoes.publish(index, source.position, self.listener.position) {
                        eprintln!("[air] {error}");
                    }
                }
            }
        }
    }

    fn publish_trace_control(&mut self) {
        let config = TraceUiConfig {
            monitor_gain_db: self.monitor_gain_db,
            stages: self.stage_mix,
            route: self.source_comparison,
            // Source trajectories are fixture identity, not discontinuities each
            // tick. Height edits are authoring changes; listener motion is measured.
            source_heights: std::array::from_fn(|index| {
                self.sources
                    .get(index)
                    .map_or(SourceHeight::Street, |source| source.height)
            }),
        };
        if self.trace_last_config != Some(config) {
            self.trace_config_epoch = self.trace_config_epoch.wrapping_add(1);
            self.trace_last_config = Some(config);
        }
        self.trace_control_writer.publish(TraceControl {
            recording: self.level_trace.is_recording(),
            generation: self.trace_generation,
            ui_config_epoch: self.trace_config_epoch,
        });
    }

    fn trace_metadata_snapshot(&self) -> serde_json::Value {
        let mix = SourceMix::from_sources(&self.sources);
        serde_json::json!({
            "generation": self.trace_generation,
            "ui_config_epoch": self.trace_config_epoch,
            "capture_context": self.capture_draft(),
            "source_monitor_gains": &mix.monitor_gains[..self.sources.len()],
            "source_retrigger_generations": &mix.retrigger_generations[..self.sources.len()],
            "route": format!("{:?}", self.source_comparison),
            "listener_enu_m": [self.listener.position.east_m, self.listener.position.north_m, self.listener.position.up_m],
            "source_positions_enu_m": self.sources.iter().map(|source| [source.position.east_m, source.position.north_m, source.position.up_m]).collect::<Vec<_>>(),
            "snapshot_scope": "UI observation only; positions and controls are not a sample-exact adoption log"
        })
    }

    fn level_trace_controls(&mut self, ui: &mut egui::Ui) {
        let audio_live = match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(_) => true,
            _ => false,
        };
        if self.level_trace.controls(ui, audio_live) == crate::level_trace_ui::TraceUiAction::Record
        {
            // Survives coalesced Stop/Record UI snapshots: every new recording is
            // distinct even when the callback never observes recording=false.
            self.trace_generation = self.trace_generation.wrapping_add(1);
            self.publish_trace_control();
            if self.trace_recording_snapshots.len() == 128 {
                self.trace_recording_snapshots.pop_front();
            }
            self.trace_recording_snapshots
                .push_back(self.trace_metadata_snapshot());
        }
        let requested = self.level_trace.is_recording();
        let acknowledged = self.level_trace.reader.is_recording();
        ui.horizontal_wrapped(|ui| {
            if requested && !acknowledged { ui.small("Starting trace · awaiting audio callback"); }
            if !requested && acknowledged { ui.small("Stopping trace · awaiting final audio window"); }
            if !audio_live { ui.small("Open audio output to record; recording never starts playback"); }
            if ui.add_enabled(!requested && !acknowledged && !self.level_trace.reader.samples().is_empty(), egui::Button::new("Export level trace")).clicked() {
                // Stop acknowledgement precedes this drain, so the callback's
                // final queued partial window is included in the saved snapshot.
                self.level_trace.update();
                let metadata = serde_json::json!({
                    "recording_start_snapshots_last_128": self.trace_recording_snapshots,
                    "export_snapshot": self.trace_metadata_snapshot(),
                    "signal_scope": "Actual summed final stereo PCM after original output safety/limiter and optional quiet guard; not individual-source levels",
                    "timing_scope": "PCM windows use exact stream-local audio frames; listener pose is sample-held per callback. Input-consumed SourceMix changes break epochs. Monitor/stage/route/height changes break at UI-boundary observation, not proven sample-exact graph adoption.",
                    "phase_scope": "Source phase and acoustic causality unavailable. Moving sources, authored decay and limiter action may cause drops.",
                    "bounds": "6000 retained windows; 256 queued windows; last 128 recording-start UI snapshots; older data may be evicted and counters report this"
                });
                self.trace_export_status = Some(match self.level_trace.export_json(self.capture.root(), metadata) {
                    Ok(path) => format!("Saved {}", path.display()),
                    Err(error) => format!("Could not export trace: {error}"),
                });
            }
        });
        if let Some(status) = &self.trace_export_status {
            ui.small(status);
        }
    }

    fn is_street_comparison(&self) -> bool {
        self.capture_static.fixture_id == "astra-artillery-street-path-candidate-v1"
    }

    /// Move only: stop the previous shot, preserve heading and levels, and
    /// publish through the same safety/pose/simulation boundary as walking.
    fn choose_listening_spot(&mut self, east_m: f32) -> bool {
        if !self.is_street_comparison()
            || !matches!(self.capture_state, CaptureUiState::Idle)
            || ![434.02_f32, 438.02_f32].contains(&east_m)
        {
            return false;
        }
        self.stop_demo_sources();
        self.autopilot.enabled = false;
        self.listener.position = EnuVector3::new(east_m, 483.82, 1.5);
        self.publish_listener_control(EnuVector3::default());
        true
    }

    fn begin_song_load(&mut self, path: PathBuf, hovered: Option<usize>) -> Result<(), String> {
        if self.song_load.is_some() {
            return Err("A song is still loading; drop again when it is ready".into());
        }
        if !matches!(self.capture_state, CaptureUiState::Idle) {
            return Err("Finish the capture before dropping a song".into());
        }
        let index = song_target(&self.sources, hovered).ok_or("scene has no speaker")?;
        self.stop_demo_sources();
        let stereo = self.sources[index].stereo_program;
        let slot_rms_dbfs = self.sources[index].program_rms_dbfs;
        let decode_path = path.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new().name("song-decode".into()).spawn(move || {
            let result = prepare_song(&decode_path, stereo).map(|mut asset| {
                adapt_song_to_slot(&mut asset, slot_rms_dbfs);
                asset
            });
            let _ = sender.send(result);
        }).map_err(|error| format!("cannot start song decoder: {error}"))?;
        self.song_status = Some(format!("Loading {}…", song_file_label(&path)));
        self.song_load = Some(SongLoad { index, path, receiver });
        Ok(())
    }

    fn finish_song_load(&mut self, index: usize, path: &std::path::Path, result: Result<PreparedAsset, String>) -> Result<(), String> {
        let asset = result?;
        self.stop_demo_sources();
        let band_track = std::sync::Arc::new(crate::map_link::band_track(&asset.samples, SAMPLE_RATE));
        self.map.tracks_dirty = true;
        let source = &mut self.sources[index];
        source.band_track = Some(band_track);
        #[cfg(feature = "live-output")]
        if let AudioState::Live(output) = &mut self.audio { output.select_song(index); }
        self.song_writers[index].publish(SongBuffer {
            loaded_generation: source.retrigger_generation,
            mono: asset.samples,
            stereo: asset.stereo_samples,
        });
        source.asset_id = format!("song:{}", path.display());
        source.audition_label = song_file_label(path);
        self.speaker_songs[index] = Some(path.to_owned());
        source.onset_frames = 0;
        source.retrigger_start_delay = None;
        source.song_fallback = true;
        source.anomaly_asset_identity = asset.descriptor_sha256;
        self.overlay_identity_cache = None;
        self.anomaly_field.selected_source = index;
        self.song_status = Some(format!("{} · ready · press Play", song_file_label(path)));
        Ok(())
    }

    fn poll_song_load(&mut self) {
        for writer in &mut self.song_writers { writer.reclaim(); }
        let result = self.song_load.as_ref().and_then(|load| match load.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("song decoder stopped".into())),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        });
        if let Some(result) = result {
            let load = self.song_load.take().expect("completed decoder");
            if let Err(error) = self.finish_song_load(load.index, &load.path, result) {
                self.song_status = Some(error);
            }
        }
    }

    fn remember_drop_markers(&mut self, rect: Rect, project: impl Fn(EnuVector3) -> Option<Pos2>) {
        for (index, source) in self.sources.iter().enumerate() {
            if let Some(point) = project(source.position).filter(|point| rect.contains(*point)) {
                self.drop_markers.push((index, point));
            }
        }
    }

    fn has_live_input(&self) -> bool {
        self.sources
            .iter()
            .any(|source| source.asset_id.starts_with("live-input:") || source.song_fallback)
    }

    /// Several sounds around a music speaker, without a cue timeline: each
    /// sound is its own on/off toggle.
    fn palette(&self) -> bool {
        self.scene_cues.is_empty() && self.music_speaker.is_some()
    }

    fn listening_header(&mut self, ui: &mut egui::Ui) {
        let live = self.has_live_input();
        let scene = !self.scene_cues.is_empty();
        let palette = self.palette();
        ui.horizontal(|ui| {
            ui.heading(if scene {
                "Scene"
            } else if palette {
                "Listen"
            } else if live {
                "Live music"
            } else {
                "Artillery street"
            });
            if palette {
                ui.add_space(16.0);
                for (label, map) in [("Walk", false), ("Map", true)] {
                    if ui
                        .add(
                            egui::Button::new(label)
                                .selected(self.ground_map_enabled == map)
                                .min_size(egui::vec2(72.0, 28.0)),
                        )
                        .clicked()
                    {
                        self.ground_map_enabled = map;
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Diagnostics").clicked() {
                    self.listening_mode = false;
                }
            });
        });
        if palette {
            ui.label(
                "W A S D to walk · drag to turn · Shift to run · drop any song file on the window",
            );
            return;
        }
        if scene {
            ui.label("Press Play scene, then walk or watch the performance.");
        } else if live {
            ui.label("Press Play, then walk around the corner speaker.");
        } else {
            ui.label("Compare the same blast four metres apart");
            ui.small("Choose a spot, then Play again. Listen for a sudden loss of body.");
        }
        let selected = self.anomaly_field.selected_source;
        // After a Play, the heard gap includes the street route (T1 route timing).
        let heard_lead_s = self
            .feed_events
            .iter()
            .find(|event| event.source_index == selected)
            .and_then(|event| {
                let crack = event.crack?.arrival_time_s;
                let impact = event
                    .timeline()
                    .into_iter()
                    .find(|arrival| {
                        arrival.kind == crate::acoustic_feed::ArrivalKind::RoutedPrimary
                    })?
                    .arrival_time_s;
                Some(impact - crack)
            });
        if let Some(crack) = self
            .ballistic_cracks
            .iter_mut()
            .find(|crack| crack.parent_index == selected)
        {
            match heard_lead_s {
                Some(lead_s) => {
                    ui.small(format!(
                        "Crack: arrives {lead_s:.2} s before the impact (via street)"
                    ));
                }
                None => {
                    ui.small(crack.summary(self.listener.position));
                }
            }
        }
        ui.horizontal(|ui| {
            let editable = matches!(self.capture_state, CaptureUiState::Idle);
            for (label, east) in [("Spot A", 434.02_f32), ("Spot B", 438.02_f32)]
                .into_iter()
                .filter(|_| !live && !scene)
            {
                let selected = (self.listener.position.east_m - east).abs() < 0.05
                    && (self.listener.position.north_m - 483.82).abs() < 0.05;
                if ui
                    .add_enabled(
                        editable,
                        egui::Button::new(label)
                            .selected(selected)
                            .min_size(egui::vec2(100.0, 34.0)),
                    )
                    .clicked()
                {
                    self.choose_listening_spot(east);
                }
            }
            ui.separator();
            ui.selectable_value(&mut self.ground_map_enabled, true, "Map");
            ui.selectable_value(&mut self.ground_map_enabled, false, "Walk");
        });
    }

    fn demo_listening_controls(&mut self, ui: &mut egui::Ui) {
        let selected = self
            .anomaly_field
            .selected_source
            .min(self.sources.len().saturating_sub(1));
        let audio_live = match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(_) => true,
            _ => false,
        };
        let expired = self
            .quiet_output
            .as_ref()
            .is_some_and(|reader| reader.read().expired);
        let editable = matches!(self.capture_state, CaptureUiState::Idle);
        let palette = self.palette();
        #[cfg(feature = "live-output")]
        {
            let speaker = self.music_speaker.unwrap_or(selected);
            let song = self.speaker_songs.get(speaker).cloned().flatten();
            let song_label = song.as_deref().map(song_name);
            let picked = match &mut self.audio {
                AudioState::Live(output) => {
                    if self.sources[speaker].asset_id.starts_with("song:") {
                        output.show_song(speaker);
                    }
                    output
                        .input_picker(
                            ui,
                            speaker,
                            editable && self.song_load.is_none(),
                            song_label.as_deref(),
                        )
                        .then(|| output.song_selected(speaker))
                }
                _ => None,
            };
            match (picked, song) {
                (Some(true), Some(path)) => {
                    if let Err(error) = self.begin_song_load(path, Some(speaker)) {
                        self.song_status = Some(error);
                    }
                }
                (Some(_), _) => {
                    self.sources[speaker].enabled = false;
                    self.sources[speaker].asset_id = "live-input:app-picker".into();
                    if !palette {
                        self.sources[speaker].audition_label = "Live music · corner speaker".into();
                    }
                    self.song_writers[speaker].clear();
                    self.song_status = None;
                    self.source_mix_writer.publish(SourceMix::from_sources(&self.sources));
                    self.mix_gains_cache = None;
                }
                (None, _) => {}
            }
        }
        let can_listen = editable && !expired && self.song_load.is_none() && (audio_live || self.quiet_ready());
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    can_listen && !self.sources.is_empty(),
                    egui::Button::new(if !self.scene_cues.is_empty() {
                        "Play scene"
                    } else if palette {
                        "Play all"
                    } else if self.has_live_input() {
                        "Play"
                    } else if self.listening_mode {
                        "Play again"
                    } else {
                        "Listen to this sound"
                    })
                    .min_size(if self.listening_mode {
                        egui::vec2(112.0, 36.0)
                    } else {
                        egui::Vec2::ZERO
                    }),
                )
                .clicked()
            {
                if palette {
                    self.play_fixture_at_start();
                } else if self.scene_cues.is_empty() {
                    self.select_source_comparison(selected, SourceComparisonMode::Spatial);
                } else {
                    self.play_scene();
                }
            }
            if ui
                .add_enabled(
                    editable,
                    egui::Button::new("Stop").min_size(if self.listening_mode {
                        egui::vec2(80.0, 36.0)
                    } else {
                        egui::Vec2::ZERO
                    }),
                )
                .clicked()
            {
                self.stop_demo_sources();
            }
            let gains = self.observed_mix().gains(self.sources.len());
            let audible = gains[..self.sources.len()].iter().any(|gain| *gain > 0.0);
            let state = if expired {
                "Finished · silent".to_owned()
            } else if self.quiet_ready() && audible {
                "Ready · sound selected".to_owned()
            } else if !audio_live {
                "Output closed".to_owned()
            } else if !self.scene_cues.is_empty() && self.scene_control.running {
                let scene = self.playback_status_reader.read().scene.unwrap_or_default();
                format!("Scene · {:.1} s", scene.frame as f64 / f64::from(SAMPLE_RATE))
            } else if audible {
                let playing = self
                    .sources
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| gains[*index] > 0.0)
                    .collect::<Vec<_>>();
                let observed = self.playback_status_reader.read();
                if playing.len() == 1 {
                    let (index, source) = playing[0];
                    let state =
                        playback_status_label(observed.sources[index], source.retrigger_generation);
                    if self.listening_mode {
                        state.to_owned()
                    } else {
                        format!("{state} · {}", source.audition_label)
                    }
                } else {
                    let finished = playing
                        .iter()
                        .filter(|(index, source)| {
                            let status = observed.sources[*index];
                            status.generation == source.retrigger_generation && status.ended
                        })
                        .count();
                    format!("{} sounds enabled · {finished} finished", playing.len())
                }
            } else {
                "Stopped".to_owned()
            };
            ui.label(state);
        });
        if palette {
            let observed = self.playback_status_reader.read();
            let mut toggled = None;
            ui.horizontal_wrapped(|ui| {
                ui.label("Sounds");
                for (index, source) in self.sources.iter().enumerate() {
                    let status = observed.sources[index];
                    let finished = status.generation == source.retrigger_generation && status.ended;
                    let on = source.enabled && !finished;
                    if ui
                        .add_enabled(
                            can_listen,
                            egui::Button::new(&source.audition_label)
                                .selected(on)
                                .min_size(egui::vec2(0.0, 30.0)),
                        )
                        .on_hover_text(if on { "Turn off" } else { "Turn on" })
                        .clicked()
                    {
                        toggled = Some((index, on));
                    }
                }
            });
            if let Some((index, on)) = toggled {
                self.toggle_sound(index, on);
            }
        }
        ui.horizontal(|ui| {
            let mut enabled = self.head_tracking.enabled();
            if ui
                .checkbox(&mut enabled, "Head tracking (AirPods)")
                .changed()
            {
                self.head_tracking.set_enabled(enabled);
            }
            if ui
                .add_enabled(
                    self.head_tracking.can_recenter(),
                    egui::Button::new("Recenter"),
                )
                .clicked()
            {
                self.head_tracking.recenter();
            }
            if enabled {
                ui.small(self.head_tracking.status());
            }
        });
        let changed = !self.listening_mode
            && ui
                .horizontal(|ui| {
                    self.sources.get_mut(selected).is_some_and(|source| {
                        ui.add_enabled(
                            editable,
                            egui::Slider::new(
                                &mut source.monitor_offset_db,
                                MIN_SOURCE_OFFSET_DB..=MAX_SOURCE_OFFSET_DB,
                            )
                            .text("Sound level")
                            .suffix(" dB"),
                        )
                        .on_hover_text(
                            "Adjust the selected sound only; other sounds keep their levels.",
                        )
                        .changed()
                    })
                })
                .inner;
        if changed {
            self.source_mix_writer
                .publish(SourceMix::from_sources(&self.sources));
            self.mix_gains_cache = None;
        }
        let meter = self.meter_reader.read();
        let monitor_gain_changed = ui
            .horizontal(|ui| {
                let fraction = ((meter.peak_dbfs + 90.0) / 90.0).clamp(0.0, 1.0);
                ui.add(egui::ProgressBar::new(fraction).desired_width(110.0));
                if self.listening_mode {
                    ui.label("Volume");
                    // The same monitor gain as Diagnostics' master: inside the
                    // guarded output chain, so the limiter still applies.
                    let changed = ui
                        .add_enabled(
                            editable,
                            egui::Slider::new(
                                &mut self.monitor_gain_db,
                                MIN_MONITOR_GAIN_DB..=MAX_MONITOR_GAIN_DB,
                            )
                            .suffix(" dB")
                            .fixed_decimals(0),
                        )
                        .on_hover_text("Output safety limits stay on at every setting.")
                        .changed();
                    self.air_combo(ui);
                    changed
                } else {
                    ui.small(format!(
                        "Output signal · peak {:.1} dBFS · RMS {:.1} dBFS",
                        meter.peak_dbfs, meter.rms_dbfs
                    ));
                    false
                }
            })
            .inner;
        if monitor_gain_changed {
            self.output_safety_controller
                .set_monitor_gain_db(self.monitor_gain_db)
                .expect("the monitor-gain slider publishes only finite values");
        }
    }

    /// Size, reach and level for one source in the walk views.
    fn walk_size(&self, index: usize, air_db_per_m: f32) -> crate::walk_view::SizeInfo {
        use crate::walk_view as walk;
        let source = &self.sources[index];
        let adjustable = source.trajectory.is_none()
            && !self
                .ballistic_cracks
                .iter()
                .any(|crack| crack.parent_index == index);
        let (preset, spl_at_one_m_db, width_m) = match self
            .walk
            .presets
            .get(index)
            .copied()
            .flatten()
            .filter(|_| adjustable)
        {
            Some(preset) => (
                Some(preset),
                walk::SIZE_PRESETS[preset].spl_at_one_m_db,
                walk::SIZE_PRESETS[preset].width_m,
            ),
            None => (
                None,
                source.declared_spl_at_one_meter_db,
                self.walk
                    .authored_width_m
                    .get(index)
                    .copied()
                    .unwrap_or_default(),
            ),
        };
        walk::SizeInfo {
            preset,
            adjustable,
            spl_at_one_m_db,
            width_m,
            reach_m: walk::reach_m(spl_at_one_m_db, air_db_per_m),
        }
    }

    /// Phone-first walk-view prototypes (`FIGHTBOX_WALK_VIEW=A|B|C`). Returns
    /// the horizontal drag used to turn.
    fn walk_view(&mut self, ui: &mut egui::Ui, design: crate::walk_view::WalkDesign) -> f32 {
        use crate::walk_view::{self as walk, WalkAction, WalkDesign};
        let full = ui.max_rect();
        let portrait = full.height() > full.width();
        let quiet = self.quiet_output.as_ref().map(|reader| reader.read());
        let quiet_ready = self.quiet_ready();
        let (editable, can_listen) = self.transport_gates();
        let observed = self.playback_status_reader.read();
        let air_db_per_m = self.scene_air_exponents[1] * 8.685_89;
        let listener = [
            self.listener.position.east_m,
            self.listener.position.north_m,
        ];
        let yaw = self.listener.yaw_radians;
        let selected = self
            .anomaly_field
            .selected_source
            .min(self.sources.len().saturating_sub(1));
        let sizes = (0..self.sources.len())
            .map(|index| self.walk_size(index, air_db_per_m))
            .collect::<Vec<_>>();
        let pins = self
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let status = observed.sources[index];
                let finished = status.generation == source.retrigger_generation && status.ended;
                let size = sizes[index];
                let distance_m = vector_length(subtract(source.position, self.listener.position));
                let ground = [source.position.east_m, source.position.north_m];
                let rise = source.position.up_m - self.listener.position.up_m;
                let direction = if rise > 2.0 * walk::distance(ground, listener) {
                    "overhead"
                } else {
                    walk::relative_direction(listener, yaw, ground)
                };
                walk::Pin {
                    index,
                    label: &source.audition_label,
                    position: [
                        source.position.east_m,
                        source.position.north_m,
                        source.position.up_m,
                    ],
                    color: walk::pin_color(index),
                    on: source.enabled && !source.muted && !finished,
                    selected: index == selected,
                    moving: source.trajectory.is_some(),
                    size,
                    distance_m,
                    direction,
                    level_here_db: walk::open_air_level_db(
                        size.spl_at_one_m_db,
                        distance_m.max(size.width_m * 0.5),
                        air_db_per_m,
                    ),
                }
            })
            .collect::<Vec<_>>();
        let placing = self.walk.placing.map(|at| walk::Placing {
            at,
            covered: self.probe_coverage.coverage(EnuVector3::new(
                at[0],
                at[1],
                crate::source_drag::ground_height(&self.mesh, at) + 1.5,
            )) == crate::acoustic_state::ProbeCoverage::Covered,
            street: self.walk.atlas.street_at(at),
            distance_m: walk::distance(at, listener),
            direction: walk::relative_direction(listener, yaw, at),
        });
        let frame = walk::WalkFrame {
            map: &self.ground_map,
            atlas: &self.walk.atlas,
            listener,
            yaw,
            ground_up_m: self.walk.ground_up_m,
            pins,
            placing,
        };
        let mut actions = Vec::new();
        let mut gain_db = self.monitor_gain_db;
        let mut gain_changed = false;
        let mut start_quiet = false;
        let mut drag_x = 0.0;
        let mut place_at = None;
        let mut drops = Vec::new();

        let top = Rect::from_min_size(full.min, egui::vec2(full.width(), 64.0));
        let note = self.walk.atlas.place_note(listener);
        let facing = walk::compass_word([0.0, 0.0], [yaw.sin(), yaw.cos()]);
        walk::top_bar(ui, top, &note, facing, &mut actions);
        let body = Rect::from_min_max(Pos2::new(full.left(), top.bottom()), full.max);
        let (view, sheet) = match (design, portrait) {
            (WalkDesign::Earshot, true) => body.split_top_bottom_at_y(body.top() + body.width()),
            (WalkDesign::Earshot, false) => body.split_left_right_at_x(body.left() + body.height()),
            (_, true) => body.split_top_bottom_at_y(body.bottom() - 300.0),
            (_, false) => body.split_left_right_at_x(body.right() - 380.0),
        };
        let painter = ui.painter_at(view);
        let mut hits = Vec::<(usize, Rect)>::new();
        let mut minimap = None;
        let plan = match design {
            WalkDesign::Map => Some(walk::Plan::heading_up(
                listener,
                yaw,
                Pos2::new(view.center().x, view.top() + view.height() * 0.64),
                2.2,
            )),
            // The rim reaches just past the farthest pin or the selected reach.
            WalkDesign::Earshot => Some(walk::Plan::fisheye(
                listener,
                yaw,
                view.center(),
                view.width().min(view.height()) * 0.5 - 8.0,
                40.0,
                frame
                    .pins
                    .iter()
                    .map(|pin| {
                        if pin.selected {
                            pin.size.reach_m.max(pin.distance_m)
                        } else {
                            pin.distance_m
                        }
                    })
                    .fold(250.0_f32, f32::max)
                    .min(3000.0)
                    * 1.12,
            )),
            WalkDesign::Walk => None,
        };
        if let Some(plan) = plan {
            walk::paint_city(
                &painter,
                view,
                plan,
                &frame,
                Some(if design == WalkDesign::Map {
                    12.0
                } else {
                    10.5
                }),
            );
            if design == WalkDesign::Earshot {
                walk::paint_range_rings(
                    &painter,
                    plan,
                    view.width().min(view.height()) * 0.5 - 8.0,
                );
            }
            let order = frame
                .pins
                .iter()
                .filter(|pin| !pin.selected)
                .chain(frame.pins.iter().filter(|pin| pin.selected));
            let mut labels = walk::PinLabels::default();
            labels.reserve(Rect::from_center_size(plan.anchor, egui::vec2(24.0, 24.0)));
            let mut offscreen = Vec::new();
            for pin in order {
                match walk::paint_plan_pin(&painter, view, plan, pin, &mut labels) {
                    Some(point) => {
                        hits.push((
                            pin.index,
                            Rect::from_center_size(point, egui::vec2(30.0, 30.0)),
                        ));
                        drops.push((pin.index, point));
                    }
                    None => offscreen.push(pin),
                }
            }
            // Edge chips go last and slide clear of everything else.
            let mut taken = labels.paint(&painter);
            taken.extend(hits.iter().map(|(_, rect)| *rect));
            walk::paint_you(&painter, plan.anchor, 44.0);
            taken.push(Rect::from_center_size(plan.anchor, egui::vec2(24.0, 24.0)));
            let compass = view.right_top() + egui::vec2(-26.0, 26.0);
            walk::paint_compass(&painter, compass, plan);
            taken.push(Rect::from_center_size(compass, egui::vec2(34.0, 34.0)));
            if let Some(px_per_m) = plan.px_per_m() {
                taken.push(walk::paint_scale_bar(
                    &painter,
                    view.left_bottom() + egui::vec2(16.0, -12.0),
                    px_per_m,
                ));
                for pin in offscreen {
                    walk::paint_edge_chip(
                        &painter,
                        view.shrink(10.0),
                        plan.anchor,
                        plan.project(pin.ground()),
                        format!("{} · {}", pin.label, walk::format_distance(pin.distance_m)),
                        pin.color,
                        &mut taken,
                    );
                }
            }
            if let Some(placing) = &frame.placing {
                walk::paint_placing(&painter, plan.project(placing.at), placing);
            }
        } else {
            hits = self.draw_walk_eye(&painter, view, &frame);
            let side = if portrait { 132.0 } else { 210.0 };
            let rect = Rect::from_min_size(
                view.right_bottom() - egui::vec2(side + 12.0, side + 12.0),
                egui::vec2(side, side),
            );
            walk::paint_minimap(&painter, rect, &frame);
            minimap = Some((rect, walk::minimap_plan(rect, &frame)));
        }
        let response = ui.interact(view, ui.id().with("walk-view"), Sense::click_and_drag());
        if response.dragged_by(egui::PointerButton::Primary) {
            drag_x = ui.input(|input| input.pointer.delta().x);
        }
        if response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            if let Some((index, _)) = hits.iter().rev().find(|(_, rect)| rect.contains(pointer)) {
                actions.push(WalkAction::Select(*index));
                actions.push(WalkAction::CancelPlace);
            } else if let Some((_, plan)) = minimap.filter(|(rect, _)| rect.contains(pointer)) {
                place_at = Some(plan.unproject(pointer));
            } else {
                place_at = match plan {
                    Some(plan) => Some(plan.unproject(pointer)),
                    None => self.walk_ground_point(view, pointer),
                };
            }
        }

        ui.painter().rect_filled(sheet, 0.0, walk::BACKGROUND);
        if portrait && design != WalkDesign::Earshot {
            ui.painter().rect_filled(
                Rect::from_center_size(
                    sheet.center_top() + egui::vec2(0.0, 7.0),
                    egui::vec2(38.0, 4.0),
                ),
                2.0,
                Color32::from_rgb(60, 70, 78),
            );
        }
        let inner = sheet.shrink2(egui::vec2(16.0, 14.0));
        let transport_height = if quiet.is_some() { 64.0 } else { 36.0 };
        let (content, transport) = inner.split_top_bottom_at_y(inner.bottom() - transport_height);
        ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                    if let Some(placing) = &frame.placing {
                        walk::placing_sheet(ui, &frame, placing, &mut actions);
                    } else if design == WalkDesign::Earshot {
                        walk::loudness_list(ui, &frame, can_listen, air_db_per_m, &mut actions);
                    } else if let Some(pin) = frame.pins.iter().find(|pin| pin.selected) {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| walk::pin_heading(ui, pin));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    walk::play_button(
                                        ui,
                                        pin,
                                        can_listen,
                                        egui::vec2(104.0, 40.0),
                                        &mut actions,
                                    );
                                },
                            );
                        });
                        ui.add_space(4.0);
                        if pin.size.adjustable {
                            walk::size_row(ui, pin, air_db_per_m, &mut actions);
                        }
                        ui.label(
                            egui::RichText::new(pin.size.caption())
                                .size(11.5)
                                .color(walk::MUTED),
                        );
                        ui.add_space(4.0);
                        walk::sound_pills(ui, &frame.pins, &mut actions);
                    }
                });
        });
        ui.painter().line_segment(
            [
                transport.left_top() - egui::vec2(16.0, 4.0),
                transport.right_top() + egui::vec2(16.0, -4.0),
            ],
            Stroke::new(1.0, Color32::from_rgb(34, 42, 48)),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(transport), |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            ui.horizontal(|ui| {
                let pill = |text: &str| {
                    egui::Button::new(egui::RichText::new(text).size(13.0))
                        .min_size(egui::vec2(78.0, 32.0))
                        .corner_radius(16.0)
                };
                if ui.add_enabled(can_listen, pill("Play all")).clicked() {
                    actions.push(WalkAction::PlayAll);
                }
                if ui.add_enabled(editable, pill("Stop all")).clicked() {
                    actions.push(WalkAction::StopAll);
                }
                ui.label(egui::RichText::new("Volume").size(12.0).color(walk::MUTED));
                // The same guarded monitor gain as the classic view.
                gain_changed = ui
                    .add_enabled(
                        editable,
                        egui::Slider::new(&mut gain_db, MIN_MONITOR_GAIN_DB..=MAX_MONITOR_GAIN_DB)
                            .suffix(" dB")
                            .fixed_decimals(0),
                    )
                    .on_hover_text("Output safety limits stay on at every setting.")
                    .changed();
            });
            if let Some(status) = quiet {
                ui.horizontal(|ui| {
                    let remaining = status.limit_frames.saturating_sub(status.processed_frames)
                        as f64
                        / f64::from(SAMPLE_RATE);
                    ui.colored_label(
                        Color32::from_rgb(223, 181, 94),
                        if status.expired {
                            "Quiet · finished · silent".to_owned()
                        } else {
                            format!("Quiet · {remaining:.1} s available")
                        },
                    );
                    if quiet_ready {
                        start_quiet = ui.button("Start quiet audition").clicked();
                    }
                });
            }
        });
        drop(frame);

        self.drop_markers.extend(drops);
        if gain_changed {
            self.monitor_gain_db = gain_db;
            self.output_safety_controller
                .set_monitor_gain_db(self.monitor_gain_db)
                .expect("the monitor-gain slider publishes only finite values");
        }
        if start_quiet {
            self.start_quiet_audition();
        }
        if let Some(at) = place_at {
            self.walk.placing = Some(at);
        }
        // Prototype comparison only: V cycles the three designs.
        if ui.input(|input| input.key_pressed(egui::Key::V)) {
            let next = WalkDesign::ALL
                .iter()
                .position(|candidate| *candidate == design)
                .unwrap_or(0)
                + 1;
            self.walk.design = Some(WalkDesign::ALL[next % WalkDesign::ALL.len()]);
        }
        for action in actions {
            match action {
                WalkAction::Select(index) => self.anomaly_field.selected_source = index,
                WalkAction::Toggle { index, on } => {
                    self.anomaly_field.selected_source = index;
                    self.toggle_sound(index, on);
                }
                WalkAction::Preset { index, preset } => {
                    if let Some(slot) = self.walk.presets.get_mut(index) {
                        *slot = preset;
                    }
                }
                WalkAction::CancelPlace => self.walk.placing = None,
                WalkAction::PinHere { index, at } => self.pin_source_at(index, at),
                WalkAction::PlayAll => self.play_fixture_at_start(),
                WalkAction::StopAll => self.stop_demo_sources(),
                WalkAction::Diagnostics => self.listening_mode = false,
            }
        }
        drag_x
    }

    /// (editable, can_listen): the gates on the listening view's own Play
    /// all, Stop all and Volume, shared with the City Map link.
    fn transport_gates(&self) -> (bool, bool) {
        let audio_live = match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(_) => true,
            _ => false,
        };
        let expired = self
            .quiet_output
            .as_ref()
            .is_some_and(|reader| reader.read().expired);
        let editable = matches!(self.capture_state, CaptureUiState::Idle);
        let can_listen = self.walk_preview
            || (editable
                && !expired
                && self.song_load.is_none()
                && (audio_live || self.quiet_ready()));
        (editable, can_listen)
    }

    /// Whether a sound is audibly on: enabled, unmuted, and not a finished
    /// one-shot.
    fn source_sounding(&self, index: usize, observed: &PlaybackSnapshot) -> bool {
        let source = &self.sources[index];
        let status = observed.sources[index];
        let finished = status.generation == source.retrigger_generation && status.ended;
        source.enabled && !source.muted && !finished
    }

    /// One City Map link turn: greet newcomers, apply requests, publish
    /// state at most every 100 ms. UI thread only; non-blocking sockets.
    pub(crate) fn serve_map_link(&mut self, link: &mut crate::map_link::MapLink, package: &Path) {
        if link.accept() {
            if self.map.hello.is_none() {
                self.map.hello = Some(self.map_hello_json(package));
            }
            link.greet(self.map.hello.as_deref().unwrap_or_default());
            self.map.last_state.clear();
            // Music data goes out again for the newcomer.
            self.map.tracks_dirty = true;
            self.map.field_sent.clear();
            self.map.paths_sent.clear();
            // A new client sees shots from now on, not replays of old ones.
            for event in &self.feed_events {
                self.map
                    .shots_sent
                    .insert(event.source_index, (event.event_sequence, event.trigger_audio_sample));
            }
        }
        let ids = self
            .sources
            .iter()
            .map(|source| source.id.clone())
            .collect::<Vec<_>>();
        let ids = ids.iter().map(String::as_str).collect::<Vec<_>>();
        let keys = self
            .map
            .spots
            .iter()
            .map(|spot| spot.key.clone())
            .collect::<Vec<_>>();
        let keys = keys.iter().map(String::as_str).collect::<Vec<_>>();
        let observed = self.playback_status_reader.read();
        for command in link.read_commands() {
            let result = command
                .and_then(|command| {
                    command.resolve(&ids, &keys, MIN_MONITOR_GAIN_DB..=MAX_MONITOR_GAIN_DB)
                })
                .and_then(|action| self.apply_map_action(action, &observed));
            if let Err(text) = result {
                link.notice(&text);
            }
        }
        if link.has_clients() {
            for event in &self.feed_events {
                let key = (event.event_sequence, event.trigger_audio_sample);
                if self.map.shots_sent.get(&event.source_index) != Some(&key) {
                    self.map.shots_sent.insert(event.source_index, key);
                    link.broadcast(&crate::map_link::shot_json(
                        event,
                        event.elapsed_s(self.feed_audio_sample),
                    ));
                }
            }
        }
        if link.has_clients() {
            self.serve_map_music(link, &observed);
        }
        if link.has_clients()
            && self
                .map
                .last_sent
                .is_none_or(|sent| sent.elapsed().as_millis() >= 100)
        {
            let rms_dbfs = self.meter_reader.read().rms_dbfs;
            let state = self.map_state_json(&observed, rms_dbfs);
            if state != self.map.last_state {
                link.broadcast(&state);
                self.map.last_state = state;
            }
            self.map.last_sent = Some(Instant::now());
        }
        link.flush();
    }

    /// The map's music look: each song's band track once, its routed field
    /// over the walkable dots whenever the speaker settles somewhere new,
    /// and its paths to You as You or the speaker move (at most 5 a second).
    /// Control side only; the audio callback never sees any of it.
    fn serve_map_music(&mut self, link: &mut crate::map_link::MapLink, observed: &PlaybackSnapshot) {
        use crate::map_link::{paths_json, track_json};
        if self.map.hello.is_none() {
            return;
        }
        if std::mem::take(&mut self.map.tracks_dirty) {
            for source in &self.sources {
                if let Some(track) = &source.band_track {
                    link.broadcast(&track_json(&source.id, track));
                }
            }
        }
        let dragging = self.map.drag.as_ref().map(|drag| drag.index);
        let throttle = self
            .map
            .last_paths
            .is_some_and(|sent| sent.elapsed().as_millis() < 200);
        for index in 0..self.sources.len() {
            // Songs get their own bands; live system audio (song_fallback
            // without a track) still gets the field and paths, lit by the
            // broadband output meter the map already receives.
            if (self.sources[index].band_track.is_none() && !self.sources[index].song_fallback)
                || self.sources[index].trajectory.is_some()
                || dragging == Some(index)
            {
                continue;
            }
            let position = self.sources[index].position;
            let routed = self.map.fields.get(&index).map(|(at, _)| *at);
            if routed != Some(position) {
                let line = self.music_field_json(index);
                self.map.fields.insert(index, (position, line));
            }
            if self.map.field_sent.get(&index) != Some(&position) {
                if let Some((_, line)) = self.map.fields.get(&index) {
                    link.broadcast(line);
                }
                self.map.field_sent.insert(index, position);
            }
            let listener = self.listener.position;
            let moved = self.map.paths_sent.get(&index).is_none_or(|(at, heard)| {
                *at != position || vector_length(subtract(*heard, listener)) >= 1.0
            });
            let first = !self.map.paths_sent.contains_key(&index);
            if first || moved && !throttle && self.source_sounding(index, observed) {
                let field = self.feed_planner.route_field(position);
                let mut plan = self.feed_planner.plan(&field, listener);
                // No echo plan to here: the stream still follows the street
                // route the field itself was lit by.
                if plan.primary_polyline_enu_m.is_empty()
                    && let Some(route) = self.feed_planner.route_to(&field, listener)
                {
                    plan.primary_polyline_enu_m = route;
                }
                link.broadcast(&paths_json(
                    &self.sources[index].id,
                    [listener.east_m, listener.north_m, listener.up_m],
                    &plan,
                    self.scene_air_exponents,
                ));
                self.map.paths_sent.insert(index, (position, listener));
                self.map.last_paths = Some(Instant::now());
            }
        }
    }

    /// One music source's routed level at every walkable dot within
    /// [`MUSIC_FIELD_RADIUS_M`]: the planner's street route from the speaker,
    /// spreading, the scene's air per band and a band loss per corner.
    fn music_field_json(&self, index: usize) -> String {
        use crate::map_link::{field_json, route_band_db};
        let source = self.sources[index].position;
        let field = self.feed_planner.route_field(source);
        let ear = self.listener.position.up_m;
        let dots = self
            .map
            .hello_dots
            .iter()
            .filter(|dot| {
                (dot[0] - source.east_m).hypot(dot[1] - source.north_m) <= MUSIC_FIELD_RADIUS_M
            })
            .filter_map(|dot| {
                let path = self
                    .feed_planner
                    .route_to(&field, EnuVector3::new(dot[0], dot[1], ear))?;
                let path = path
                    .iter()
                    .map(|point| [point.east_m, point.north_m, point.up_m])
                    .collect::<Vec<_>>();
                let (band_db, length, _) = route_band_db(&path, self.scene_air_exponents);
                Some([dot[0], dot[1], band_db[0], band_db[1], band_db[2], length])
            })
            .collect::<Vec<_>>();
        field_json(
            &self.sources[index].id,
            [source.east_m, source.north_m, source.up_m],
            &dots,
        )
    }

    /// The map's once-per-client scene description: geo origin, quality
    /// dots, suggested spots and sound ids.
    fn map_hello_json(&mut self, package: &Path) -> String {
        use crate::map_link::{PROTOCOL, SceneGeo, SpotInputs, quality_dots, suggest_spots};
        let geo = SceneGeo::read(package);
        let bounds = self.ground_map.bounds;
        let access = self
            .walk
            .atlas
            .streets
            .iter()
            .map(|street| street.points.clone())
            .collect::<Vec<_>>();
        let mut dots = quality_dots(&self.probe_points, bounds, &self.ground_map.roofs, &access);
        self.map.footprints = geo
            .as_ref()
            .map(|geo| geo.buildings.iter().map(|building| building.ring.clone()).collect())
            .unwrap_or_default();
        crate::map_link::drop_inside_footprints(&mut dots, &self.map.footprints);
        self.map.hello_dots = dots.points.clone();
        let street = dots
            .points
            .iter()
            .filter(|point| point[2] >= 1.0)
            .map(|point| [point[0], point[1]])
            .collect::<Vec<_>>();
        let roofs = self
            .mesh
            .triangles
            .iter()
            .filter_map(|triangle| {
                let [a, b, c] = triangle.map(|index| self.mesh.vertices_enu_m[index as usize]);
                let low = a.up_m.min(b.up_m).min(c.up_m);
                let high = a.up_m.max(b.up_m).max(c.up_m);
                (high - low < 0.1 && low > 0.5).then(|| {
                    (
                        [
                            (a.east_m + b.east_m + c.east_m) / 3.0,
                            (a.north_m + b.north_m + c.north_m) / 3.0,
                        ],
                        high,
                    )
                })
            })
            .collect::<Vec<_>>();
        let atlas = &self.walk.atlas;
        let corners = atlas
            .corners
            .iter()
            .map(|(at, [a, b])| (*at, format!("{} & {}", atlas.names[*a], atlas.names[*b])))
            .collect::<Vec<_>>();
        let top = |at: crate::ground_map::Point| crate::source_drag::ground_height(&self.mesh, at);
        let covered = |point: [f32; 3]| {
            self.probe_coverage
                .coverage(EnuVector3::new(point[0], point[1], point[2]))
                == crate::acoustic_state::ProbeCoverage::Covered
        };
        let spots = suggest_spots(&SpotInputs {
            listener: [self.listener.position.east_m, self.listener.position.north_m],
            bounds,
            roofs: &roofs,
            street: &street,
            landmarks: geo.as_ref().map_or(&[], |geo| geo.landmarks.as_slice()),
            towers: geo.as_ref().map_or(&[], |geo| geo.towers.as_slice()),
            corners: &corners,
            top: &top,
            covered: &covered,
            street_at: &|at| atlas.street_at(at),
        });
        let hello = serde_json::json!({
            "type": "hello",
            "protocol": PROTOCOL,
            "origin": geo.as_ref().map(|geo| serde_json::json!({
                "latitude_deg": geo.latitude_deg,
                "longitude_deg": geo.longitude_deg,
            })),
            "bounds_m": [bounds.0, bounds.1],
            "volume": { "min_db": MIN_MONITOR_GAIN_DB, "max_db": MAX_MONITOR_GAIN_DB },
            "dots": dots,
            "spots": spots,
            "buildings": geo.as_ref().map_or(&[][..], |geo| geo.buildings.as_slice()),
            "streets": self.walk.atlas.streets.iter().map(|street| serde_json::json!({
                "kind": format!("{:?}", street.kind).to_lowercase(),
                "points": street.points.iter()
                    .map(|p| [(p[0] * 10.0).round() / 10.0, (p[1] * 10.0).round() / 10.0])
                    .collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "sources": self.sources.iter().enumerate().map(|(index, source)| {
                let color = crate::walk_view::pin_color(index);
                serde_json::json!({
                    "id": source.id,
                    "label": source.audition_label,
                    "color": [color.r(), color.g(), color.b()],
                    "moving": source.trajectory.is_some(),
                })
            }).collect::<Vec<_>>(),
        })
        .to_string();
        self.map.spots = spots;
        hello
    }

    /// Listener, sounds and transport, for the map's live view.
    fn map_state_json(&self, observed: &PlaybackSnapshot, rms_dbfs: f32) -> String {
        use crate::walk_view as walk;
        let round = |value: f32| (f64::from(value) * 100.0).round() / 100.0;
        let (editable, can_listen) = self.transport_gates();
        let air_db_per_m = self.scene_air_exponents[1] * 8.685_89;
        let listener = [self.listener.position.east_m, self.listener.position.north_m];
        let yaw = self.listener.yaw_radians;
        let note = self.walk.atlas.place_note(listener);
        let selected = self
            .anomaly_field
            .selected_source
            .min(self.sources.len().saturating_sub(1));
        let sources = self
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let size = self.walk_size(index, air_db_per_m);
                // What you hear includes the sound's level trim (a loud spot
                // raises it), so level and reach do too.
                let spl_db = size.spl_at_one_m_db + source.monitor_offset_db;
                let reach_m = walk::reach_m(spl_db, air_db_per_m);
                let boost_db = self
                    .map
                    .boosts
                    .get(&index)
                    .map_or(0.0, |(before, _)| source.monitor_offset_db - before);
                let distance_m = vector_length(subtract(source.position, self.listener.position));
                let ground = [source.position.east_m, source.position.north_m];
                // The move rule: the surface below must be baked; height is
                // free. Flying sounds follow their own baked flight path.
                let top = crate::source_drag::ground_height(&self.mesh, ground);
                let covered = source.trajectory.is_some()
                    || crate::map_link::landing(ground, top, source.position.up_m - top, |point| {
                        self.probe_coverage
                            .coverage(EnuVector3::new(point[0], point[1], point[2]))
                            == crate::acoustic_state::ProbeCoverage::Covered
                    })
                    .is_some();
                // A song's playhead, for the map's music look (seconds).
                let playhead_s = source.band_track.as_ref().map(|track| {
                    let seconds = observed.sources[index].cursor as f64 / f64::from(SAMPLE_RATE);
                    (seconds % f64::from(track.length_s.max(0.001)) * 100.0).round() / 100.0
                });
                serde_json::json!({
                    "playhead_s": playhead_s,
                    "id": source.id,
                    "position_m": [
                        round(source.position.east_m),
                        round(source.position.north_m),
                        round(source.position.up_m),
                    ],
                    "on": self.source_sounding(index, observed),
                    "covered": covered,
                    "size": size.label(),
                    "spl_db": round(spl_db),
                    "boost_db": round(boost_db),
                    "width_m": round(size.width_m),
                    "reach_m": f64::from(reach_m.round()),
                    "distance_m": round(distance_m),
                    "direction": walk::relative_direction(listener, yaw, ground),
                    "level_db": walk::open_air_level_db(
                        spl_db,
                        distance_m.max(size.width_m * 0.5),
                        air_db_per_m,
                    )
                    .round() as f64,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "type": "state",
            "listener": {
                "position_m": [
                    round(self.listener.position.east_m),
                    round(self.listener.position.north_m),
                    round(self.listener.position.up_m),
                ],
                "yaw_deg": round(yaw.to_degrees()),
            },
            "place": { "here": note.here, "near": note.near },
            "facing": walk::compass_word([0.0, 0.0], [yaw.sin(), yaw.cos()]),
            "selected": self.sources.get(selected).map(|source| source.id.as_str()),
            "volume_db": round(self.monitor_gain_db),
            // Output level after the limiter, for the map's music pulse.
            "meter": {
                "rms_dbfs": rms_dbfs.max(-120.0).round() as f64,
            },
            "editable": editable,
            "can_play": can_listen,
            "sources": sources,
        })
        .to_string()
    }

    fn apply_map_action(
        &mut self,
        action: crate::map_link::MapAction,
        observed: &PlaybackSnapshot,
    ) -> Result<(), String> {
        use crate::map_link::MapAction;
        let (editable, can_listen) = self.transport_gates();
        match action {
            MapAction::Select { index } => self.anomaly_field.selected_source = index,
            MapAction::SetOn { index, on } => {
                let sounding = self.source_sounding(index, observed);
                if on && !sounding && !can_listen {
                    return Err("Audio isn't ready to play yet".into());
                }
                self.anomaly_field.selected_source = index;
                if on != sounding {
                    self.toggle_sound(index, sounding);
                }
            }
            MapAction::Fire { index } => {
                if !can_listen {
                    return Err("Audio isn't ready to play yet".into());
                }
                // The same arm-and-retrigger as turning it on: a fresh shot.
                self.anomaly_field.selected_source = index;
                self.toggle_sound(index, false);
            }
            MapAction::MoveYou { at } => {
                if !editable {
                    return Err("Finish the capture first".into());
                }
                let access = self
                    .walk
                    .atlas
                    .streets
                    .iter()
                    .map(|street| street.points.clone())
                    .collect::<Vec<_>>();
                let at = crate::map_link::you_landing(
                    at,
                    &access,
                    &self.ground_map.roofs,
                    &self.map.footprints,
                )?;
                // Like walking there: heading, height and levels stay; the
                // same safety, pose and simulation publish as the walk keys.
                self.autopilot.enabled = false;
                self.listener.position =
                    EnuVector3::new(at[0], at[1], self.listener.position.up_m);
                self.publish_listener_control(EnuVector3::default());
            }
            MapAction::PlayAll if can_listen => self.play_fixture_at_start(),
            MapAction::PlayAll => return Err("Audio isn't ready to play yet".into()),
            MapAction::StopAll
            | MapAction::SetVolume { .. }
            | MapAction::Move { .. }
            | MapAction::Spot { .. }
                if !editable =>
            {
                return Err("Finish the capture first".into());
            }
            MapAction::StopAll => self.stop_demo_sources(),
            MapAction::SetVolume { db } => {
                // Already clamped to the slider's range; output safety stays on.
                self.monitor_gain_db = db;
                self.output_safety_controller
                    .set_monitor_gain_db(db)
                    .map_err(|error| format!("volume refused: {error:?}"))?;
            }
            MapAction::Move {
                index,
                at,
                above_top_m,
                done,
            } => {
                // A loud spot's boost ends the moment the sound moves on.
                self.restore_map_boost(index);
                return self.map_move(index, at, above_top_m, done);
            }
            MapAction::Spot { index, spot } => {
                let spot = self.map.spots[spot].clone();
                let at = if spot.follows_listener {
                    [self.listener.position.east_m, self.listener.position.north_m]
                } else {
                    [spot.east_m, spot.north_m]
                };
                self.restore_map_boost(index);
                self.map_move(index, at, Some(spot.above_top_m), true)?;
                if let Some(spl_db) = spot.loud_spl_db {
                    self.boost_map_level(index, spl_db);
                }
            }
        }
        Ok(())
    }

    /// Raises a sound's level trim toward `spl_db` at 1 m, within the trim
    /// slider's own range; never lowers it. The output limiter is untouched.
    fn boost_map_level(&mut self, index: usize, spl_db: f32) {
        let source = &mut self.sources[index];
        let before = source.monitor_offset_db;
        let wanted = clamp_source_offset_db(
            (spl_db - source.declared_spl_at_one_meter_db).clamp(0.0, MAX_SOURCE_OFFSET_DB),
        );
        if wanted <= before {
            return;
        }
        source.monitor_offset_db = wanted;
        self.map.boosts.insert(index, (before, wanted));
        self.publish_map_mix();
    }

    /// Puts back a loud spot's trim, unless it was changed by hand since.
    fn restore_map_boost(&mut self, index: usize) {
        let Some((before, boosted)) = self.map.boosts.remove(&index) else {
            return;
        };
        if self.sources[index].monitor_offset_db == boosted {
            self.sources[index].monitor_offset_db = before;
            self.publish_map_mix();
        }
    }

    /// The same publish as the trim slider's.
    fn publish_map_mix(&mut self) {
        self.source_comparison = None;
        self.monitor_route_controller.select_spatial();
        self.source_mix_writer
            .publish(SourceMix::from_sources(&self.sources));
        self.mix_gains_cache = None;
    }

    /// A map drag step or a spot pick. Same rules as the map drag in the
    /// classic view: static sounds only, baked spots only (holding at the
    /// last one otherwise), replans at most every 100 ms and on release.
    fn map_move(
        &mut self,
        index: usize,
        at: crate::ground_map::Point,
        above_top_m: Option<f32>,
        done: bool,
    ) -> Result<(), String> {
        let label = self.sources[index].audition_label.clone();
        if self.sources[index].trajectory.is_some() {
            return Err(format!("{label} follows its own flight path"));
        }
        let current = self.sources[index].position;
        let above = above_top_m.unwrap_or_else(|| {
            current.up_m
                - crate::source_drag::ground_height(&self.mesh, [current.east_m, current.north_m])
        });
        let top = crate::source_drag::ground_height(&self.mesh, at);
        let landed = crate::map_link::landing(at, top, above, |point| {
            self.probe_coverage
                .coverage(EnuVector3::new(point[0], point[1], point[2]))
                == crate::acoustic_state::ProbeCoverage::Covered
        });
        let position = landed.map_or(current, |[east, north, up]| EnuVector3::new(east, north, up));
        let drag = match &mut self.map.drag {
            Some(drag) if drag.index == index => drag,
            slot => slot.insert(MapDrag {
                index,
                planned: current,
                last_plan: Instant::now(),
            }),
        };
        let replan =
            drag.planned != position && (done || drag.last_plan.elapsed().as_millis() >= 100);
        if replan {
            drag.planned = position;
            drag.last_plan = Instant::now();
        }
        self.update_source_position(index, position);
        self.simulation.publish_update(SimulationUpdate {
            listener: self.listener.listener_state(EnuVector3::default()),
            sources: self.source_motion,
        });
        if replan {
            self.replan_placed_source(index);
        }
        self.anomaly_field.selected_source = index;
        if done {
            self.map.drag = None;
            self.sources[index].street_height_m = position.up_m;
            self.sources[index].height = SourceHeight::Street;
            let id = self.sources[index].id.clone();
            self.scene_save_status = self
                .scene_positions
                .set_position(&id, [position.east_m, position.north_m, position.up_m])
                .err();
            if landed.is_none() {
                return Err(format!("No baked path there; {label} stays at its last baked spot"));
            }
        }
        Ok(())
    }

    /// Moves a static source to a tapped spot at its current height above
    /// ground, with the same coverage rule and replan as a map drag.
    fn pin_source_at(&mut self, index: usize, at: crate::ground_map::Point) {
        self.walk.placing = None;
        let source = &self.sources[index];
        let above_ground = source.street_height_m
            - crate::source_drag::ground_height(
                &self.mesh,
                [source.position.east_m, source.position.north_m],
            );
        let position = EnuVector3::new(
            at[0],
            at[1],
            crate::source_drag::ground_height(&self.mesh, at) + above_ground,
        );
        if source.trajectory.is_some()
            || self.probe_coverage.coverage(position)
                != crate::acoustic_state::ProbeCoverage::Covered
        {
            return;
        }
        self.update_source_position(index, position);
        self.simulation.publish_update(SimulationUpdate {
            listener: self.listener.listener_state(EnuVector3::default()),
            sources: self.source_motion,
        });
        self.replan_placed_source(index);
        self.sources[index].street_height_m = position.up_m;
        self.sources[index].height = SourceHeight::Street;
        self.anomaly_field.selected_source = index;
        let id = self.sources[index].id.clone();
        self.scene_save_status = self
            .scene_positions
            .set_position(&id, [position.east_m, position.north_m, position.up_m])
            .err();
    }

    fn walk_ground_point(&self, rect: Rect, pointer: Pos2) -> Option<crate::ground_map::Point> {
        FirstPersonProjection::new(
            self.listener.position,
            self.listener.yaw_radians,
            FIRST_PERSON_VERTICAL_FOV_RADIANS,
            FIRST_PERSON_NEAR_M,
        )
        .ground_point(pointer, rect, self.walk.ground_up_m)
    }

    /// B: the first-person street with road paint, corner signs and pins.
    fn draw_walk_eye(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        frame: &crate::walk_view::WalkFrame<'_>,
    ) -> Vec<(usize, Rect)> {
        use crate::walk_view as walk;
        let painter = painter.with_clip_rect(rect);
        let horizon = rect.center().y;
        let mut sky = egui::Mesh::default();
        for (point, color) in [
            (rect.left_top(), Color32::from_rgb(17, 23, 31)),
            (rect.right_top(), Color32::from_rgb(17, 23, 31)),
            (
                Pos2::new(rect.left(), horizon),
                Color32::from_rgb(52, 63, 74),
            ),
            (
                Pos2::new(rect.right(), horizon),
                Color32::from_rgb(52, 63, 74),
            ),
        ] {
            sky.colored_vertex(point, color);
        }
        sky.add_triangle(0, 1, 2);
        sky.add_triangle(1, 3, 2);
        painter.add(egui::Shape::mesh(sky));
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(rect.left(), horizon), rect.max),
            0.0,
            Color32::from_rgb(30, 36, 36),
        );
        let projection = FirstPersonProjection::new(
            self.listener.position,
            self.listener.yaw_radians,
            FIRST_PERSON_VERTICAL_FOV_RADIANS,
            FIRST_PERSON_NEAR_M,
        );
        let (mut ground, mut solid): (Vec<_>, Vec<_>) = self
            .faces
            .iter()
            .zip(&self.face_colors)
            .filter_map(|(face, &fill)| {
                let projected = project_face(
                    &self.mesh,
                    *face,
                    [
                        projection.eye.east_m,
                        projection.eye.north_m,
                        projection.eye.up_m,
                    ],
                    fill,
                    rect,
                    |point| projection.camera_point(point),
                    |point, rect| projection.screen_point(point, rect),
                )?;
                (!projected_face_fully_outside(&projected, rect, FACE_CULL_MARGIN_PX))
                    .then_some((face.is_ground, projected))
            })
            .partition(|(is_ground, _)| *is_ground);
        let mut ground = ground.drain(..).map(|(_, face)| face).collect::<Vec<_>>();
        let mut solid = solid.drain(..).map(|(_, face)| face).collect::<Vec<_>>();
        paint_faces(&painter, &mut ground);
        let point = |point: [f32; 3]| {
            projection
                .project_point(EnuVector3::new(point[0], point[1], point[2]), rect)
                .map(|(screen, _)| screen)
        };
        let polygon = |points: &[[f32; 3]]| {
            let camera = points
                .iter()
                .map(|point| projection.camera_point(EnuVector3::new(point[0], point[1], point[2])))
                .collect::<Vec<_>>();
            let clipped = clip_to_near_plane(&camera, 0.5);
            if clipped.len() < 3 {
                return None;
            }
            let screen = clipped
                .iter()
                .map(|point| projection.screen_point(*point, rect))
                .collect::<Vec<_>>();
            Rect::from_points(&screen)
                .intersects(rect.expand(4.0))
                .then_some(screen)
        };
        let eye = walk::Eye {
            point: &point,
            polygon: &polygon,
        };
        walk::paint_ground_decals(&painter, frame, &eye);
        paint_faces(&painter, &mut solid);
        walk::paint_corner_signs(&painter, rect, frame, &eye);
        if let Some(placing) = &frame.placing
            && let Some(at) = point([placing.at[0], placing.at[1], frame.ground_up_m])
        {
            walk::paint_placing(&painter, at, placing);
        }
        walk::paint_walk_pins(
            &painter,
            rect,
            frame,
            &eye,
            self.listener.yaw_radians.sin_cos(),
        )
    }

    fn update_control(&mut self, ctx: &egui::Context, drag_delta_x: f32) {
        self.release_ballistic_cracks();
        if let Some(echoes) = &mut self.host_echoes {
            echoes.report_transfer_fallbacks();
        }
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
        self.publish_listener_control(velocity);
    }

    fn publish_listener_control(&mut self, velocity: EnuVector3) {
        if !self.scene_cues.is_empty() {
            if self.scene_control.running && self.scene_prepared_listener.is_none_or(|previous| {
                vector_length(subtract(previous, self.listener.position)) >= 0.25
            }) {
                self.prepare_scene_sources();
            }
            self.scene_control.listener = self.listener.position;
            self.scene_control_writer.publish(self.scene_control);
        }
        for crack in &mut self.ballistic_cracks {
            if !crack.is_looping() {
                continue;
            }
            let enabled = if self.scene_cues.is_empty() {
                self.sources[crack.parent_index].enabled
            } else {
                self.scene_control.running
            };
            if !enabled {
                self.source_motion[crack.slot_index].active = false;
                continue;
            }
            match crack.follow_listener(self.listener.position) {
                Ok(Some(shot)) => {
                    if let Some(armed) = shot.crack {
                        if let Err(error) = self.output_safety_controller.set_source(crack.slot_index, &armed.profile, None) {
                            eprintln!("[gunfire] motion safety calibration rejected: {error:?}");
                            continue;
                        }
                        self.source_motion[crack.slot_index].pose.position = armed.profile.pose.position;
                        self.source_motion[crack.slot_index].active = true;
                    }
                }
                Ok(None) => {}
                Err(error) => eprintln!("[gunfire] motion plan retained: {error}"),
            }
        }
        if let Some(echoes) = &mut self.host_echoes {
            echoes.follow_listener(self.listener.position);
        }
        let mut listener = self.listener.listener_state(velocity);
        self.head_tracking.apply(&mut listener, self.listener.yaw_radians);
        self.output_safety_controller
            .set_listener_position(listener.pose.position)
            .expect("workbench listener controls remain finite");
        self.pose_mailbox.publish(listener);
        self.simulation.publish_update(SimulationUpdate {
            listener,
            sources: self.source_motion,
        });
    }

    fn draw_scene(&mut self, painter: &egui::Painter, rect: Rect) {
        let camera = self.camera;
        self.remember_drop_markers(rect, |position| camera.project(position, rect));
        let painter = painter.with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(13, 18, 24));
        let mut faces = self
            .faces
            .iter()
            .zip(&self.face_colors)
            .filter_map(|(face, &fill)| {
                let projected = project_face(
                    &self.mesh,
                    *face,
                    self.camera.eye,
                    fill,
                    rect,
                    |point| self.camera.camera_point(point),
                    |point, rect| self.camera.screen_point(point, rect),
                )?;
                (!projected_face_fully_outside(&projected, rect, FACE_CULL_MARGIN_PX))
                    .then_some(projected)
            })
            .collect::<Vec<_>>();
        paint_faces(&painter, &mut faces);
        self.draw_anomaly_overlay(&painter, rect);
        for source in &self.sources {
            self.draw_map_trajectory(&painter, rect, source);
        }
        for source in &self.sources {
            if let Some(point) = self.camera.project(source.position, rect) {
                painter.circle_filled(point, 5.0, Color32::from_rgb(255, 174, 66));
                painter.text(
                    point + egui::vec2(8.0, -8.0),
                    egui::Align2::LEFT_BOTTOM,
                    &source.audition_label,
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

    fn interact_map_sources(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        bounds: (crate::ground_map::Point, crate::ground_map::Point),
    ) {
        let projection = crate::ground_map::MapProjection::new(bounds, rect);
        for index in 0..self.sources.len() {
            let source = &self.sources[index];
            let point = projection.project([source.position.east_m, source.position.north_m]);
            if !rect.contains(point)
                && self
                    .source_drag
                    .as_ref()
                    .is_none_or(|drag| drag.index != index)
            {
                continue;
            }
            let reason = if source.trajectory.is_some() {
                Some("Authored trajectory · cannot drag this source")
            } else if self
                .ballistic_cracks
                .iter()
                .any(|crack| crack.parent_index == index)
            {
                Some("Authored ballistic flight · impact position is fixed")
            } else {
                None
            };
            let response = ui
                .interact(
                    Rect::from_center_size(point, egui::vec2(24.0, 24.0)),
                    ui.id().with(("source-position", index)),
                    if reason.is_some() {
                        Sense::hover()
                    } else {
                        Sense::drag()
                    },
                )
                .on_hover_text(reason.unwrap_or("Drag to place this source · height stays fixed"));
            if response.drag_started_by(egui::PointerButton::Primary) {
                let origin = ui.input(|input| input.pointer.press_origin());
                if let Some(origin) = origin {
                    self.source_drag = Some(crate::source_drag::SourceDrag::new(
                        index,
                        bounds,
                        projection,
                        origin,
                        source.position,
                        source.street_height_m
                            - crate::source_drag::ground_height(
                                &self.mesh,
                                [source.position.east_m, source.position.north_m],
                            ),
                    ));
                    self.anomaly_field.selected_source = index;
                    self.scene_save_status = None;
                }
            }
        }
        if self.source_drag.is_some() {
            let (pointer, released, down) = ui.input(|input| {
                (
                    input.pointer.interact_pos(),
                    input.pointer.button_released(egui::PointerButton::Primary),
                    input.pointer.button_down(egui::PointerButton::Primary),
                )
            });
            if let Some(pointer) = pointer {
                self.update_source_drag(pointer, released || !down);
            } else if !down {
                // Losing the pointer/focus commits only the last covered pose.
                let drag = self.source_drag.as_ref().unwrap();
                let pointer = drag.pointer_for_last_covered();
                self.update_source_drag(pointer, true);
            }
        }
    }

    fn draw_ground_map(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect) {
        let mut selected = self
            .anomaly_field
            .selected_source
            .min(self.sources.len().saturating_sub(1));
        let Some(source) = self.sources.get(selected) else {
            return;
        };
        let listener = [
            self.listener.position.east_m,
            self.listener.position.north_m,
        ];
        let view_bounds = if let Some(drag) = &self.source_drag {
            drag.bounds
        } else if self.ground_map_whole_scene {
            self.ground_map.bounds
        } else {
            self.ground_map_local_frame.bounds(
                selected,
                [source.position.east_m, source.position.north_m],
                listener,
            )
        };
        let map_rect = Rect::from_min_max(
            rect.min + egui::vec2(28.0, 80.0),
            rect.max - egui::vec2(28.0, 64.0),
        );
        self.interact_map_sources(ui, map_rect, view_bounds);
        selected = self.anomaly_field.selected_source;
        let source = &self.sources[selected];
        let elevated = source.position.up_m > 3.0;
        if !elevated && !self.listening_mode {
            self.ground_map
                .update(selected, [source.position.east_m, source.position.north_m]);
        }
        let telemetry = self.acoustic_telemetry.read();
        let measured = if telemetry.known {
            let visibility = telemetry.source_occlusion[selected]
                .map(|value| format!("{:.0}%", value * 100.0))
                .unwrap_or_else(|| "—".to_owned());
            let path = telemetry.source_path_sh_energy[selected]
                .map(|value| format!("{value:.2e}"))
                .unwrap_or_else(|| "—".to_owned());
            format!(
                "Selected source / direct visibility {visibility} · path strength {path} (solver state)"
            )
        } else {
            "Selected-source solver state unavailable · no measured field displayed".to_owned()
        };
        let markers = self
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| crate::ground_map::Marker {
                position: [source.position.east_m, source.position.north_m],
                label: &source.audition_label,
                selected: index == selected,
                enabled: source.enabled && !source.muted,
            })
            .collect::<Vec<_>>();
        let forward = self.listener.forward();
        let event = self
            .feed_events
            .iter()
            .find(|event| event.source_index == selected);
        crate::ground_map::paint(
            painter,
            rect,
            crate::ground_map::LiveMapScene {
                diagnostic: !self.listening_mode,
                map: &self.ground_map,
                view_bounds,
                markers: &markers,
                listener: [
                    self.listener.position.east_m,
                    self.listener.position.north_m,
                ],
                forward: [forward.east_m, forward.north_m],
                elevated,
                active: source.enabled && !source.muted,
                phase_s: if event.is_some() {
                    -1.0
                } else {
                    self.audio_block_reader.read() as f32 * BLOCK_SIZE as f32 / SAMPLE_RATE as f32
                },
                selected_label: &source.audition_label,
                telemetry: &measured,
            },
        );
        // Drop targets use the same uniform north-up projection as ground_map::paint.
        if map_rect.is_positive() {
            let projection = crate::ground_map::MapProjection::new(view_bounds, map_rect);
            for (index, source) in self.sources.iter().enumerate() {
                let point = projection.project([source.position.east_m, source.position.north_m]);
                if map_rect.contains(point) { self.drop_markers.push((index, point)); }
            }
        }
        let timeline_rect = Rect::from_min_max(
            Pos2::new(map_rect.left(), rect.bottom() - 124.0),
            Pos2::new(map_rect.right(), rect.bottom() - 8.0),
        );
        if let Some(event) = event {
            crate::acoustic_view::paint_map(
                painter,
                map_rect,
                timeline_rect,
                crate::ground_map::MapProjection::new(view_bounds, map_rect),
                event,
                self.feed_audio_sample,
            );
        }
        if !self.listening_mode && map_rect.width() >= 1.0 && map_rect.height() >= 1.0 {
            let projection = crate::ground_map::MapProjection::new(view_bounds, map_rect);
            let time_rect = Rect::from_min_max(
                Pos2::new(map_rect.left(), map_rect.top()),
                Pos2::new(map_rect.right(), map_rect.top() + 44.0),
            );
            self.level_trace.paint(
                &painter.with_clip_rect(map_rect),
                map_rect,
                |position| Some(projection.project(position)),
                time_rect,
            );
        }
        if let Some(drag) = &self.source_drag {
            let point = crate::ground_map::MapProjection::new(view_bounds, map_rect)
                .project([drag.candidate.east_m, drag.candidate.north_m]);
            let color = if drag.covered {
                Color32::from_rgb(71, 220, 130)
            } else {
                Color32::from_rgb(255, 82, 82)
            };
            let map_painter = painter.with_clip_rect(map_rect);
            map_painter.circle_filled(point, 5.0, color);
            map_painter.circle_stroke(point, 11.0, Stroke::new(2.0, color));
            map_painter.text(
                point + egui::vec2(15.0, -15.0),
                egui::Align2::LEFT_BOTTOM,
                format!(
                    "{:.1}, {:.1} m · height {:.1} m\n{} · z {:.1} m",
                    drag.candidate.east_m,
                    drag.candidate.north_m,
                    drag.height_above_ground_m,
                    if drag.covered {
                        "Covered"
                    } else {
                        "No bake coverage · drop snaps back"
                    },
                    drag.candidate.up_m
                ),
                egui::FontId::monospace(12.0),
                color,
            );
        }
    }

    fn draw_audition_map_inset(&mut self, painter: &egui::Painter, rect: Rect) {
        let camera = self.camera;
        self.remember_drop_markers(rect, |position| camera.project(position, rect));
        let painter = painter.with_clip_rect(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(13, 18, 24));
        let mut faces = self
            .faces
            .iter()
            .zip(&self.face_colors)
            .filter_map(|(face, &fill)| {
                let projected = project_face(
                    &self.mesh,
                    *face,
                    self.camera.eye,
                    fill,
                    rect,
                    |point| self.camera.camera_point(point),
                    |point, rect| self.camera.screen_point(point, rect),
                )?;
                (!projected_face_fully_outside(&projected, rect, FACE_CULL_MARGIN_PX))
                    .then_some(projected)
            })
            .collect::<Vec<_>>();
        paint_faces(&painter, &mut faces);
        for source in &self.sources {
            if let Some(point) = self.camera.project(source.position, rect) {
                let color = if source.enabled {
                    Color32::from_rgb(255, 174, 66)
                } else {
                    Color32::from_rgb(105, 136, 153)
                };
                painter.circle_filled(point, if source.enabled { 5.0 } else { 2.5 }, color);
            }
        }
        let listener = self.listener.position;
        let arrow_end = add(listener, scale(self.listener.forward(), 4.0));
        if let (Some(origin), Some(end)) = (
            self.camera.project(listener, rect),
            self.camera.project(arrow_end, rect),
        ) {
            painter.circle_filled(origin, 4.0, Color32::from_rgb(64, 211, 176));
            painter.arrow(
                origin,
                end - origin,
                Stroke::new(2.0, Color32::from_rgb(64, 211, 176)),
            );
        }
        painter.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0, Color32::from_rgb(105, 136, 153)),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.left_top() + egui::vec2(8.0, 7.0),
            egui::Align2::LEFT_TOP,
            "LOCAL MAP · SYNTHETIC ENU · 585 m",
            egui::FontId::monospace(10.0),
            Color32::from_rgb(180, 202, 214),
        );
        painter.text(
            rect.left_top() + egui::vec2(8.0, 22.0),
            egui::Align2::LEFT_TOP,
            "NOT GEOREFERENCED",
            egui::FontId::monospace(9.0),
            Color32::from_rgb(255, 185, 92),
        );
    }

    fn draw_audition_macro_strip(&self, painter: &egui::Painter, rect: Rect) {
        let audition = self
            .audition
            .as_ref()
            .expect("audition macro strip requires metadata");
        if audition.mode != "gamma_audition" {
            return;
        }
        let lane = Rect::from_min_max(
            egui::pos2(rect.left() + 18.0, rect.bottom() - 64.0),
            egui::pos2(rect.right() - 18.0, rect.bottom() - 14.0),
        );
        painter.rect_filled(lane, 4.0, Color32::from_rgba_unmultiplied(6, 10, 14, 225));
        let line_y = lane.bottom() - 12.0;
        painter.line_segment(
            [
                egui::pos2(lane.left() + 18.0, line_y),
                egui::pos2(lane.right() - 18.0, line_y),
            ],
            Stroke::new(1.5, Color32::from_rgb(105, 136, 153)),
        );
        painter.text(
            egui::pos2(lane.left() + 10.0, lane.top() + 7.0),
            egui::Align2::LEFT_TOP,
            "MACRO TRANSPORT · LOCAL PROXIES · NOT MAP SCALE",
            egui::FontId::monospace(9.0),
            Color32::from_rgb(180, 202, 214),
        );
        for (index, (range, fraction)) in audition
            .macro_ranges_m
            .iter()
            .zip([0.14_f32, 0.48, 0.88])
            .enumerate()
        {
            let x = egui::lerp(lane.left()..=lane.right(), fraction);
            painter.circle_filled(egui::pos2(x, line_y), 4.0, Color32::from_rgb(255, 174, 66));
            let label = match index {
                0 => "100 m · 1.292 s",
                1 => "1 km · 3.915 s",
                _ => "10 km · 30.155 s",
            };
            debug_assert_eq!(*range, [100, 1_000, 10_000][index]);
            painter.text(
                egui::pos2(x, line_y - 6.0),
                egui::Align2::CENTER_BOTTOM,
                label,
                egui::FontId::monospace(9.0),
                Color32::from_rgb(255, 213, 146),
            );
        }
    }

    fn draw_anomaly_overlay(&mut self, painter: &egui::Painter, rect: Rect) {
        if !self.anomaly_field.overlay_enabled {
            return;
        }
        let selected = self
            .anomaly_field
            .selected_source
            .min(self.sources.len().saturating_sub(1));
        // The identity is fully determined by the selected source's position,
        // the listener height, and the runtime-mutable grid spacing on top of
        // startup-immutable inputs, so it is recomputed only when one of those
        // changes instead of every repaint.
        let identity_key = (
            selected,
            self.sources[selected].position,
            self.listener.position.up_m,
            self.anomaly_field.spacing_m.to_bits(),
        );
        if self.overlay_identity_cache.as_ref().map(|(key, _)| *key) != Some(identity_key) {
            let identity = self
                .anomaly_field
                .identity(&self.anomaly_source_query(selected), identity_key.2);
            self.overlay_identity_cache = Some((identity_key, identity));
        }
        let current = &self
            .overlay_identity_cache
            .as_ref()
            .expect("cache just populated")
            .1;
        let mut mesh = egui::Mesh::default();
        let mut hovered = None;
        let pointer = painter.ctx().pointer_hover_pos();
        if let Some(layer) = &self.anomaly_field.field {
            let stale = layer.is_stale(&current) || self.anomaly_field.stale_reason().is_some();
            for cell in &layer.cells {
                if cell.score <= 0.0 && cell.flags.is_empty() {
                    continue;
                }
                let half = layer.grid.spacing_m * 0.5;
                let min_east = (cell.position_enu.x - half).max(layer.grid.min_enu[0]);
                let max_east = (cell.position_enu.x + half).min(layer.grid.max_enu[0]);
                let min_north = (cell.position_enu.y - half).max(layer.grid.min_enu[1]);
                let max_north = (cell.position_enu.y + half).min(layer.grid.max_enu[1]);
                let corners = [
                    EnuVector3::new(min_east, min_north, cell.position_enu.z),
                    EnuVector3::new(max_east, min_north, cell.position_enu.z),
                    EnuVector3::new(max_east, max_north, cell.position_enu.z),
                    EnuVector3::new(min_east, max_north, cell.position_enu.z),
                ];
                let projected = corners.map(|corner| self.camera.project(corner, rect));
                let [Some(a), Some(b), Some(c), Some(d)] = projected else {
                    continue;
                };
                let color = anomaly_cell_color(*cell, stale);
                let first = mesh.vertices.len() as u32;
                for point in [a, b, c, d] {
                    mesh.colored_vertex(point, color);
                }
                mesh.add_triangle(first, first + 1, first + 2);
                mesh.add_triangle(first, first + 2, first + 3);
                if let Some(pointer) = pointer
                    && point_in_quad(pointer, [a, b, c, d])
                {
                    hovered = Some(*cell);
                }
            }
        }
        if let Some(trail) = &self.anomaly_field.trail {
            for cell in &trail.cells {
                let center = EnuVector3::new(
                    cell.position_enu.x,
                    cell.position_enu.y,
                    cell.position_enu.z + 0.15,
                );
                let half = 1.0;
                let corners = [
                    add(center, EnuVector3::new(-half, -half, 0.0)),
                    add(center, EnuVector3::new(half, -half, 0.0)),
                    add(center, EnuVector3::new(half, half, 0.0)),
                    add(center, EnuVector3::new(-half, half, 0.0)),
                ];
                let projected = corners.map(|corner| self.camera.project(corner, rect));
                let [Some(a), Some(b), Some(c), Some(d)] = projected else {
                    continue;
                };
                let color = anomaly_cell_color(*cell, false);
                let first = mesh.vertices.len() as u32;
                for point in [a, b, c, d] {
                    mesh.colored_vertex(point, color);
                }
                mesh.add_triangle(first, first + 1, first + 2);
                mesh.add_triangle(first, first + 2, first + 3);
            }
        }
        if !mesh.vertices.is_empty() {
            painter.add(egui::Shape::mesh(mesh));
            let stale = self.anomaly_field.stale_reason().is_some()
                || self
                    .anomaly_field
                    .field
                    .as_ref()
                    .is_some_and(|layer| layer.is_stale(&current));
            painter.text(
                rect.left_bottom() + egui::vec2(8.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                if stale {
                    "SHADOW + WEAK PATH · STALE"
                } else {
                    "SHADOW + WEAK PATH · LIVE TRAIL"
                },
                egui::FontId::monospace(9.0),
                if stale {
                    Color32::from_rgb(255, 172, 90)
                } else {
                    Color32::from_rgb(182, 202, 211)
                },
            );
        }
        if let Some(cell) = hovered {
            painter.text(
                rect.left_top() + egui::vec2(8.0, 24.0),
                egui::Align2::LEFT_TOP,
                format!(
                    "ENU {:.1}, {:.1}, {:.1} · loss {:.1} dB · path {:.1} dB · free {:.1} dB · {}",
                    cell.position_enu.x,
                    cell.position_enu.y,
                    cell.position_enu.z,
                    cell.direct_loss_db,
                    cell.path_strength_db,
                    cell.free_field_db,
                    anomaly_ids(cell),
                ),
                egui::FontId::monospace(9.0),
                Color32::WHITE,
            );
        }
    }

    fn anomaly_field_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Anomaly field");
        ui.checkbox(
            &mut self.anomaly_field.overlay_enabled,
            "Show SHADOW + WEAK PATH",
        );
        let previous_source = self.anomaly_field.selected_source;
        ui.horizontal(|ui| {
            ui.label("source");
            egui::ComboBox::from_id_salt("anomaly-field-source")
                .selected_text(&self.sources[self.anomaly_field.selected_source].id)
                .show_ui(ui, |ui| {
                    for (index, source) in self.sources.iter().enumerate() {
                        ui.selectable_value(
                            &mut self.anomaly_field.selected_source,
                            index,
                            &source.id,
                        );
                    }
                });
        });
        if self.anomaly_field.selected_source != previous_source {
            self.anomaly_field.invalidate("selected source changed");
        }
        let previous_spacing = self.anomaly_field.spacing_m;
        ui.horizontal(|ui| {
            ui.label("grid spacing");
            ui.add(
                egui::DragValue::new(&mut self.anomaly_field.spacing_m)
                    .range(2.0..=32.0)
                    .speed(1.0)
                    .suffix(" m"),
            );
            if ui.button("Run proxy sweep").clicked() {
                let source = self.anomaly_source_query(self.anomaly_field.selected_source);
                self.anomaly_field
                    .start_sweep(source, self.listener.position.up_m);
            }
        });
        if self.anomaly_field.spacing_m.to_bits() != previous_spacing.to_bits() {
            self.anomaly_field.invalidate("grid spacing changed");
        }
        ui.checkbox(
            &mut self.anomaly_field.trail_enabled,
            "Record persistent live trail (5 Hz)",
        );
        ui.checkbox(
            &mut self.anomaly_field.adaptive_enabled,
            "Densify near occlusion transitions (≤5 samples/s)",
        );
        ui.monospace(&self.anomaly_field.status);
        if let Some(field) = &self.anomaly_field.field {
            ui.small(format!(
                "proxy {}×{} · {} cells · sequential risk ramp",
                field.grid.width(),
                field.grid.height(),
                field.cells.len()
            ));
        }
        if let Some(trail) = &self.anomaly_field.trail {
            ui.small(format!(
                "trail {} samples · {} adaptive",
                trail.cells.len(),
                trail.adaptive_cells
            ));
        }
        ui.small(
            "Warm = deep shadow + weak baked fill; magenta = computation anomaly. This is a susceptibility proxy, not reflected-energy measurement.",
        );
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
            .zip(&self.face_colors)
            .filter_map(|(face, &fill)| {
                let projected = project_face(
                    &self.mesh,
                    *face,
                    [
                        projection.eye.east_m,
                        projection.eye.north_m,
                        projection.eye.up_m,
                    ],
                    fill,
                    rect,
                    |point| projection.camera_point(point),
                    |point, rect| projection.screen_point(point, rect),
                )?;
                (!projected_face_fully_outside(&projected, rect, FACE_CULL_MARGIN_PX))
                    .then_some(projected)
            })
            .collect::<Vec<_>>();
        paint_faces(&painter, &mut faces);
        for source in &self.sources {
            self.draw_first_person_trajectory(&painter, rect, projection, source);
        }
        let mut edges = [0.0_f32; 3];
        for source in &self.sources {
            match projection
                .project_point(source.position, rect)
                .filter(|(point, _)| rect.contains(*point))
            {
                Some((point, distance)) => {
                    let radius = (32.0 / distance.max(1.0)).clamp(2.5, 10.0);
                    painter.circle_filled(point, radius, Color32::from_rgb(255, 174, 66));
                    painter.text(
                        point + egui::vec2(radius + 3.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        &source.audition_label,
                        egui::FontId::monospace(10.0),
                        Color32::from_rgb(255, 213, 146),
                    );
                }
                None => {
                    // Off screen: point to the side you would turn toward.
                    let camera = projection.camera_point(source.position);
                    let across = camera[0].hypot(camera[2]);
                    let metres = across.hypot(camera[1]);
                    // More than 45 degrees up reads as overhead, not as a turn.
                    let side = if camera[1] > across {
                        2
                    } else {
                        usize::from(camera[0] >= 0.0)
                    };
                    let row = &mut edges[side];
                    let y = rect.top() + if side == 2 { 12.0 } else { 48.0 } + *row;
                    *row += 18.0;
                    let label = &source.audition_label;
                    let (text, x, align) = match side {
                        0 => (format!("◀ {label} · {metres:.0} m"), rect.left() + 8.0, egui::Align2::LEFT_CENTER),
                        1 => (format!("{label} · {metres:.0} m ▶"), rect.right() - 8.0, egui::Align2::RIGHT_CENTER),
                        _ => (format!("▲ {label} · {metres:.0} m up"), rect.center().x, egui::Align2::CENTER_CENTER),
                    };
                    painter.text(
                        Pos2::new(x, y),
                        align,
                        text,
                        egui::FontId::monospace(10.0),
                        Color32::from_rgba_unmultiplied(255, 213, 146, if source.enabled { 235 } else { 140 }),
                    );
                }
            }
        }
        painter.text(
            rect.left_top() + egui::vec2(8.0, 7.0),
            egui::Align2::LEFT_TOP,
            "LISTENER VIEW",
            egui::FontId::monospace(10.0),
            Color32::from_rgb(142, 173, 188),
        );
        if self.audition.is_some() {
            painter.text(
                rect.left_top() + egui::vec2(8.0, 23.0),
                egui::Align2::LEFT_TOP,
                "WASD move · drag to look · Shift sprint",
                egui::FontId::monospace(9.0),
                Color32::from_rgb(180, 202, 214),
            );
        }
        if let Some(event) = self
            .feed_events
            .iter()
            .find(|event| event.source_index == self.anomaly_field.selected_source)
        {
            crate::acoustic_view::paint_first_person(
                &painter,
                rect,
                event,
                self.feed_audio_sample,
                self.listener.yaw_radians,
            );
        }
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
            quiet_audition: self.quiet_provenance("capture_start"),
        }
    }

    fn quiet_provenance(&self, snapshot_phase: &str) -> Option<serde_json::Value> {
        let status = self.quiet_output.as_ref()?.read();
        Some(serde_json::json!({
            "mode": "post_graph_quiet_guard", "snapshot_phase": snapshot_phase,
            "sample_ceiling_dbfs": QUIET_CEILING_DBFS, "fade_seconds": 0.1,
            "sample_rate_hz": SAMPLE_RATE, "limit_frames": status.limit_frames,
            "processed_frames": status.processed_frames, "expired": status.expired,
            "engagement_frames": status.engagement_frames, "nonfinite_frames": status.nonfinite_frames,
            "deadline_semantics": "audio-frame budget shared across scene rebuilds; expired output remains silent; no automatic rearm",
            "capture_position": "after RuntimeGraph and quiet guard; same guarded arrays passed to meter, capture and device",
            "non_claims": ["Conservative nonlinear audition bound, not transparent engine-only PCM or a physical loudness guarantee.",
                "Latest block-published counters; independent atomics are not a sample-exact snapshot."]
        }))
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
                    late_blocks: telemetry.late_blocks,
                    processing_errors: telemetry.processing_errors,
                    stream_errors: telemetry.stream_errors,
                    snapshot_stale: telemetry.faults.snapshot_stale,
                    graph_deadline_miss: telemetry.faults.deadline_miss,
                    backend_render_error: telemetry.faults.backend_render_error,
                }
            }
            AudioState::Stopped | AudioState::Unavailable(_) => CaptureEndStats::default(),
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
            AudioState::Stopped | AudioState::Unavailable(_) => false,
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
                // Evidence captures remain normal engine output. A/B RAW is a
                // listening monitor route and is intentionally not captured.
                self.monitor_route_controller.select_spatial();
                if let Some(comparison) = &mut self.source_comparison {
                    comparison.mode = SourceComparisonMode::Spatial;
                }
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

    fn audition_panel(&mut self, ui: &mut egui::Ui) {
        let audition = self
            .audition
            .clone()
            .expect("audition panel requires metadata");
        ui.heading(&audition.title);
        ui.small(&audition.place_label);
        ui.colored_label(Color32::from_rgb(255, 185, 92), &audition.place_nonclaim);
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong(match &self.audio {
                #[cfg(feature = "live-output")]
                AudioState::Live(_) => "OUTPUT · LIVE",
                AudioState::Stopped => "OUTPUT · OFF",
                AudioState::Unavailable(_) => "OUTPUT · ERROR",
            });
            ui.separator();
            let route = match &self.audio {
                #[cfg(feature = "live-output")]
                AudioState::Live(output) => output.device_name(),
                AudioState::Stopped | AudioState::Unavailable(_) => &self.output_device_label,
            };
            ui.monospace(format!("route · {route}"));
            ui.separator();
            ui.monospace("all sources start OFF");
        });
        let audio_live = match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(_) => true,
            AudioState::Stopped | AudioState::Unavailable(_) => false,
        };
        let quiet_ready = self.quiet_ready();
        let mix_controls_enabled =
            (audio_live || quiet_ready) && matches!(self.capture_state, CaptureUiState::Idle);
        if !audio_live && !quiet_ready {
            ui.colored_label(
                Color32::from_rgb(255, 185, 92),
                "Tasting is disabled: relaunch with explicit --start-audio.",
            );
        }
        let monitor_gain_changed = ui
            .horizontal(|ui| {
                ui.label("master");
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
                self.air_combo(ui);
                changed
            })
            .inner;
        if monitor_gain_changed {
            self.output_safety_controller
                .set_monitor_gain_db(self.monitor_gain_db)
                .expect("the monitor-gain slider publishes only finite values");
        }
        let meter = self.meter_reader.read();
        ui.horizontal(|ui| {
            ui.monospace(format!("peak {:6.1} dBFS", meter.peak_dbfs));
            ui.monospace(format!("RMS {:6.1} dBFS", meter.rms_dbfs));
        });
        ui.small(if audition.mode == "squad_palette" {
            "Taste one private source at a time. Adjust each row’s source level; finite one-shots re-arm after STOP."
        } else {
            "Taste one row at a time. Adjust each row’s source level; γ4, γ9, and γ10 are sequential A/B pairs."
        });
        ui.separator();

        let mut source_mix_changed = false;
        for (source_index, source) in self.sources.iter_mut().enumerate() {
            let card = if audition.mode == "squad_palette" {
                format!("S{:02}", source_index + 1)
            } else {
                gamma_card_label(&source.id).to_owned()
            };
            let active_color = Color32::from_rgb(75, 219, 166);
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.monospace(card);
                    if ui
                        .add_enabled(
                            mix_controls_enabled,
                            egui::Button::new(if source.enabled {
                                "■ STOP"
                            } else {
                                "▶ TASTE"
                            })
                            .selected(source.enabled),
                        )
                        .clicked()
                    {
                        source.enabled = !source.enabled;
                        source.muted = false;
                        source.soloed = false;
                        source_mix_changed = true;
                    }
                    if source.enabled {
                        ui.colored_label(active_color, "PLAYING");
                    }
                    ui.strong(&source.audition_label);
                    if source.asset_id.starts_with("squad-") {
                        ui.small("SQUAD");
                    } else {
                        ui.small("CONTROL");
                    }
                    if let Some(range_m) = source.macro_range_m {
                        ui.monospace(if range_m >= 1_000 {
                            format!("{} km macro", range_m / 1_000)
                        } else {
                            format!("{range_m} m macro")
                        });
                    }
                });
                ui.horizontal(|ui| {
                    ui.small("source level");
                    if ui
                        .add_enabled(
                            mix_controls_enabled,
                            egui::Slider::new(
                                &mut source.monitor_offset_db,
                                MIN_SOURCE_OFFSET_DB..=MAX_SOURCE_OFFSET_DB,
                            )
                            .step_by(0.5)
                            .suffix(" dB")
                            .show_value(true),
                        )
                        .on_hover_text(
                            "Per-source audition trim only; changes monitor playback, not source physics",
                        )
                        .changed()
                    {
                        source.monitor_offset_db =
                            clamp_source_offset_db(source.monitor_offset_db);
                        source_mix_changed = true;
                    }
                    if ui
                        .add_enabled(
                            mix_controls_enabled && source.monitor_offset_db != 0.0,
                            egui::Button::new("Reset").small(),
                        )
                        .clicked()
                    {
                        source.monitor_offset_db = 0.0;
                        source_mix_changed = true;
                    }
                });
                ui.small(if audition.mode == "squad_palette" {
                    palette_note(&source.id)
                } else {
                    tasting_note(&source.id)
                });
            });
        }
        if source_mix_changed {
            self.source_comparison = None;
            self.monitor_route_controller.select_spatial();
            self.source_mix_writer
                .publish(SourceMix::from_sources(&self.sources));
            self.mix_gains_cache = None;
        }
        ui.separator();
        ui.collapsing("Technical detail", |ui| {
            let safety = audio_safety_telemetry(&self.audio);
            engagement_row(
                ui,
                "proximity ceiling",
                safety.as_ref().map(|value| value.proximity_ceiling_engagements),
            );
            engagement_row(
                ui,
                "true-peak limiter",
                safety.as_ref().map(|value| value.limiter_engagements),
            );
            ui.small("Private Squad derivatives are audition inputs only; no redistribution grant or physical-SPL claim.");
            ui.separator();
            self.capture_panel(ui);
        });
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
        self.anomaly_field_panel(ui);
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
                self.air_combo(ui);
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
        let mut comparison_requested = None;
        let source_comparison = self.source_comparison;
        let source_height_levels = self.source_height_levels;
        for (index, source) in self.sources.iter_mut().enumerate() {
            ui.add_enabled_ui(mix_controls_enabled, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !source.asset_id.starts_with("live-input:"),
                            egui::Checkbox::new(&mut source.enabled, ""),
                        )
                        .on_hover_text("Use Play and Stop for live music; enable other sources here")
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
                    ui.monospace(source.spl_label.as_str())
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
                        ui.small(ARTILLERY_RETRIGGER_LABEL.as_str());
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
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    ui.small("A/B monitor");
                    let raw_selected = source_comparison == Some(SourceComparison {
                        source_index: index,
                        mode: SourceComparisonMode::Raw,
                    });
                    if ui
                        .selectable_label(raw_selected, "RAW")
                        .on_hover_text(RAW_A_B_ROUTE_HOVER.as_str())
                        .clicked()
                    {
                        comparison_requested = Some((index, SourceComparisonMode::Raw));
                    }
                    let spatial_selected = source_comparison == Some(SourceComparison {
                        source_index: index,
                        mode: SourceComparisonMode::Spatial,
                    });
                    if ui
                        .selectable_label(spatial_selected, "SPATIAL")
                        .on_hover_text(
                            "Normal processed engine path. Selecting either A/B route enables, unmutes, and solos this source.",
                        )
                        .clicked()
                    {
                        comparison_requested = Some((index, SourceComparisonMode::Spatial));
                    }
                });
            });
            acoustic_badge_row(ui, &mut source.badge_text, &source.occlusion_label);
        }
        if let Some((index, height)) = source_height_changed {
            self.apply_source_height(index, height);
        }
        if let Some((index, mode)) = comparison_requested {
            self.select_source_comparison(index, mode);
            source_mix_changed = false;
        }
        if source_mix_changed {
            self.source_mix_writer
                .publish(SourceMix::from_sources(&self.sources));
            self.mix_gains_cache = None;
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
                for (index, input) in output.input_telemetry() {
                    ui.monospace(format!("input {index}: {:.1} ms · underruns {} · overruns {} · ratio {:.6} · errors {}", input.fill_ms, input.underruns, input.overruns, input.ratio, input.stream_errors));
                }
                fault_rows(ui, telemetry.faults);
            }
            AudioState::Stopped => {
                ui.colored_label(Color32::from_rgb(125, 205, 255), "Audio not started");
                ui.label("Relaunch with explicit --start-audio only after playback is authorized.");
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
        self.poll_song_load();
        #[cfg(feature = "live-output")]
        if let AudioState::Live(output) = &mut self.audio { output.poll_inputs(); }
        self.drop_markers.clear();
        self.refresh_acoustic_feed();
        self.level_trace.update();
        self.update_source_motion();
        self.refresh_acoustic_state();
        self.anomaly_field.poll();
        let selected = self
            .anomaly_field
            .selected_source
            .min(self.sources.len().saturating_sub(1));
        let source = self.anomaly_source_query(selected);
        let acoustic = self.sources[selected].acoustic;
        let energy = self.live_stage_energy.read();
        self.anomaly_field.observe_live(
            Instant::now(),
            self.listener.position,
            source,
            acoustic,
            energy,
        );
        self.update_capture_lifecycle();
        if !self.listening_mode {
            egui::SidePanel::right("performance")
                .resizable(true)
                .default_width(if self.audition.is_some() {
                    430.0
                } else {
                    340.0
                })
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if self.audition.is_some() {
                            self.audition_panel(ui);
                        } else {
                            self.perf_panel(ui);
                        }
                    });
                });
        }
        let mut drag_delta_x = 0.0;
        let walk_design = self.walk.design.filter(|_| self.listening_mode);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(
                if self.listening_mode && walk_design.is_none() { 16.0 } else { 0.0 },
            ))
            .show(ctx, |ui| {
                if let Some(design) = walk_design {
                    drag_delta_x = self.walk_view(ui, design);
                    return;
                }
                if self.listening_mode {
                    ui.spacing_mut().item_spacing = egui::vec2(10.0, 8.0);
                    self.listening_header(ui);
                    self.demo_listening_controls(ui);
                } else {
                    if (self.is_street_comparison() || self.has_live_input())
                        && ui.button("Back to listening").clicked()
                    {
                        self.listening_mode = true;
                        self.ground_map_enabled = false;
                        self.ground_map_whole_scene = false;
                    }
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.ground_map_enabled, false, "FIRST PERSON");
                        ui.selectable_value(&mut self.ground_map_enabled, true, "LIVE2D");
                        let selected = self
                            .anomaly_field
                            .selected_source
                            .min(self.sources.len().saturating_sub(1));
                        if let Some(source) = self.sources.get(selected) {
                            egui::ComboBox::from_id_salt("live2d-source")
                                .selected_text(&source.audition_label)
                                .show_ui(ui, |ui| {
                                    for (index, source) in self.sources.iter().enumerate() {
                                        ui.selectable_value(
                                            &mut self.anomaly_field.selected_source,
                                            index,
                                            &source.audition_label,
                                        );
                                    }
                                });
                        }
                    });
                    self.demo_listening_controls(ui);
                    self.level_trace_controls(ui);
                    if self.ground_map_enabled {
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut self.ground_map_whole_scene,
                                false,
                                "Local sound",
                            );
                            ui.selectable_value(
                                &mut self.ground_map_whole_scene,
                                true,
                                "Whole scene",
                            );
                        });
                    }
                }
                if !self.listening_mode || self.scene_positions.is_dirty() {
                    self.scene_save_controls(ui);
                }
                if let Some(reader) = &self.quiet_output {
                    let status = reader.read();
                    let remaining = status.limit_frames.saturating_sub(status.processed_frames)
                        as f64
                        / f64::from(SAMPLE_RATE);
                    let ready = self.quiet_ready();
                    let state = if status.expired {
                        "Finished · silent".to_owned()
                    } else if ready {
                        format!("Ready · {remaining:.1} seconds available")
                    } else if let AudioState::Unavailable(error) = &self.audio {
                        format!("Could not start · {error}")
                    } else {
                        format!("{remaining:.1} seconds left")
                    };
                    let mut start_requested = false;
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            Color32::from_rgb(223, 181, 94),
                            format!("Quiet · {state}"),
                        );
                        if ready {
                            start_requested = ui.button("Start quiet audition").clicked();
                        }
                    });
                    ui.small("Quiet limit may soften impacts");
                    if start_requested {
                        self.start_quiet_audition();
                    }
                }
                if !self.palette() {
                    ui.small("Drop a song")
                        .on_hover_text("Drop onto a map source, otherwise the first live/music source (or first source); then press Play.");
                }
                if let Some(status) = &self.song_status { ui.small(status); }
                let (response, painter) =
                    ui.allocate_painter(ui.available_size(), if self.ground_map_enabled { Sense::hover() } else { Sense::click_and_drag() });
                if self.ground_map_enabled {
                    self.draw_ground_map(ui, &painter, response.rect);
                    return;
                }
                if response.dragged_by(egui::PointerButton::Primary) {
                    drag_delta_x = ui.input(|input| input.pointer.delta().x);
                }
                self.draw_first_person(&painter, response.rect);
                if self.listening_mode {
                    return;
                }
                let pip_rect = if self.audition.is_some() {
                    audition_map_rect(response.rect)
                } else {
                    picture_in_picture_rect(response.rect)
                };
                if self.audition.is_some() {
                    self.draw_audition_map_inset(&painter, pip_rect);
                    self.draw_audition_macro_strip(&painter, response.rect);
                } else {
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
                        "LOCAL MAP",
                        egui::FontId::monospace(10.0),
                        Color32::from_rgb(142, 173, 188),
                    );
                }
            });
        let dropped = ctx.input(|input| input.raw.dropped_files.first().and_then(|file| file.path.clone()));
        if let Some(path) = dropped {
            let pointer = ctx.input(|input| input.pointer.latest_pos());
            let hovered = pointer.and_then(|pointer| self.drop_markers.iter()
                .filter(|(_, point)| point.distance(pointer) <= 14.0)
                .min_by(|(_, a), (_, b)| a.distance(pointer).total_cmp(&b.distance(pointer)))
                .map(|(index, _)| *index));
            if let Err(error) = self.begin_song_load(path, hovered) { self.song_status = Some(error); }
        }
        self.update_control(ctx, drag_delta_x);
        self.publish_trace_control();
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
            if let Some(link) = &mut self.map_link {
                active.serve_map_link(link, &self.args.package);
            }
            if let Some(fixture) = active.saved_fixture.take() {
                self.scenes[self.active_scene_index].fixture = fixture;
            }
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

fn gamma_card_label(source_id: &str) -> &str {
    let suffix = source_id.strip_prefix("gamma").unwrap_or(source_id);
    let number = suffix.split('-').next().unwrap_or("?");
    match number {
        "0" => "γ0 ",
        "1" => "γ1 ",
        "2" => "γ2 ",
        "3" => "γ3 ",
        "4" => "γ4 ",
        "5" => "γ5 ",
        "6" => "γ6 ",
        "7" => "γ7 ",
        "8" => "γ8 ",
        "9" => "γ9 ",
        "10" => "γ10",
        _ => "γ? ",
    }
}

fn palette_note(source_id: &str) -> &'static str {
    if source_id.contains("a10-impacts") {
        "Authored impact sequence; taste cadence, depth, and tail separation."
    } else if source_id.contains("a10-pass") {
        "Motion is authored into the recording; do not infer a second live trajectory."
    } else if source_id.contains("abrams") {
        "Heavy idle bed; taste low-frequency weight without masking the spatial image."
    } else if source_id.contains("tent-flap") {
        "Quiet close-detail texture; check edge definition and ambience."
    } else if source_id.contains("dshk") {
        "Heavy burst texture; compare rhythmic identity against M2, not just level."
    } else if source_id.contains("building-large") {
        "Broad fire bed; listen for stable enclosure and non-repetitive texture."
    } else if source_id.contains("fire-car") {
        "Smaller vehicle-fire bed; compare scale and spectral density against building fire."
    } else if source_id.contains("radio-static") {
        "Diffuse communications texture; check image stability and fatigue."
    } else if source_id.contains("generator") {
        "Stationary mechanical anchor; check tonal steadiness and corner behavior."
    } else if source_id.contains("m2-blast") {
        "Finite single blast; STOP then TASTE to re-arm from onset."
    } else if source_id.contains("m2-burst") {
        "Burst texture; compare articulation and spacing against DShK."
    } else if source_id.contains("mi8") {
        "Close rotor loop; listen for modulation continuity and low-end control."
    } else if source_id.contains("thunder-distant-03") {
        "Finite pressure-edge alternate; compare the transient with thunder 15."
    } else if source_id.contains("thunder-distant-15") {
        "Finite long-tail preference; taste weather-like decay and width."
    } else if source_id.contains("ural") {
        "Medium vehicle idle; compare engine character and spatial anchoring with Abrams."
    } else {
        "Private local source; taste continuity, image, texture, and fatigue."
    }
}

fn tasting_note(source_id: &str) -> &'static str {
    if source_id.starts_with("gamma0-100m") {
        "Immediate crack/body; use as the short-range timing anchor."
    } else if source_id.starts_with("gamma0-1km") {
        "Listen for a clearly later single arrival; no duplicate handoff."
    } else if source_id.starts_with("gamma0-10km") {
        "Macro context: expect about 30.15 s before one arrival; not a live 10 km map source."
    } else if source_id.starts_with("gamma1") {
        "Artillery control: edge, low body, and distance character should stay integrated."
    } else if source_id.starts_with("gamma2") {
        "Elevated firework control: height and tail should read above, not at street level."
    } else if source_id.starts_with("gamma3") {
        "Supersonic control: one snap followed by one boom, with no reset."
    } else if source_id.contains("pressure-edge") {
        "A/B 1: faster authored thunder edge; compare against the longer tail."
    } else if source_id.contains("long-tail") {
        "A/B 2: preferred weather-like decay; avoid a generic one-shot bang impression."
    } else if source_id.starts_with("gamma5") {
        "Authored A-10 pass: approach / closest pass / recession are baked in; static 125 m proxy, no second motion path."
    } else if source_id.contains("m2-contention") {
        "Squad M2 texture: taste separation and reservation stability; not the retained four-source contention proof."
    } else if source_id.contains("dshk-contention") {
        "γ6 layer B: DShK counter-layer; check separation rather than loudness."
    } else if source_id.starts_with("gamma7") {
        "Owner-home aperture evidence stem: exterior/closed/open/doorway transitions; not a new live route proof."
    } else if source_id.starts_with("gamma8") {
        "Tom’s Diner walk evidence stem: presentation control; canonical source package remains blocked."
    } else if source_id.contains("neutral") {
        "γ9 A/B monitor stem: neutral path, folded to mono then spatialized for audition."
    } else if source_id.contains("combined-once") {
        "γ9 evidence stem: five stages already composed once; audition path is not canonical directional proof."
    } else if source_id.starts_with("gamma10") {
        "Retained production-cell seam control; compare continuity, not macro distance."
    } else {
        "Listen for continuity, coherent image, and absence of clicks or resets."
    }
}

fn tune_reflection_workers(config: &mut fightbox_steam_audio::S3SimulationConfig, source_count: usize) {
    // Keep the existing fixture-based worker count; crack companions share it.
    if source_count > 1 {
        let workers = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(4)
            .min(source_count) as i32;
        config.simulation_threads = config.simulation_threads.max(workers);
    }
}

fn audio_safety_telemetry(audio: &AudioState) -> Option<SafetyTelemetry> {
    match audio {
        #[cfg(feature = "live-output")]
        AudioState::Live(output) => Some(output.telemetry().safety),
        AudioState::Stopped | AudioState::Unavailable(_) => None,
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
fn acoustic_badge_row(ui: &mut egui::Ui, badge: &mut BadgeTextCache, occlusion_label: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(22.0);
        ui.colored_label(badge_color(badge.probe_tone()), badge_text(badge.probe()));
        for (text, tone) in badge.chips() {
            ui.colored_label(badge_color(*tone), badge_text(text));
        }
    })
    .response
    .on_hover_text(ACOUSTIC_BADGE_HOVER);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(22.0);
        ui.colored_label(
            badge_color(badge.quality_tone()),
            badge_text(badge.quality()),
        );
        ui.colored_label(badge_color(BadgeTone::Ok), badge_text(occlusion_label));
    });
    ui.horizontal(|ui| {
        ui.add_space(22.0);
        ui.colored_label(
            badge_color(badge.path_tone()),
            badge_text(badge.path_diagnostics()),
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

fn badge_text(text: &str) -> egui::RichText {
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

    fn capture_spatial_block(&mut self, _audible: bool) {}
}

impl ListenerStateSink for RuntimeGraph {
    fn set_listener_state(&mut self, listener: ListenerState) {
        RuntimeGraph::set_listener_state(self, listener);
    }
}

enum SceneRenderer {
    Binaural(RuntimeGraph),
    Ambix(crate::spatial_export::SpatialExportGraph),
}

impl ListenerStateSink for SceneRenderer {
    fn set_listener_state(&mut self, listener: ListenerState) {
        match self {
            Self::Binaural(graph) => graph.set_listener_state(listener),
            Self::Ambix(graph) => graph.set_listener_state(listener),
        }
    }

    fn capture_spatial_block(&mut self, audible: bool) {
        if let Self::Ambix(graph) = self { graph.capture_block(audible); }
    }
}

impl BlockProcessor for SceneRenderer {
    fn block_size_frames(&self) -> usize {
        match self {
            Self::Binaural(graph) => graph.block_size_frames(),
            Self::Ambix(graph) => graph.block_size_frames(),
        }
    }

    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
        match self {
            Self::Binaural(graph) => graph.process_block(block),
            Self::Ambix(graph) => graph.process_block(block),
        }
    }

    fn process_program_block(&mut self, block: fightbox_runtime::ProgramProcessBlock<'_>) -> Result<(), RenderError> {
        match self {
            Self::Binaural(graph) => graph.process_program_block(block),
            Self::Ambix(graph) => graph.process_program_block(block),
        }
    }

    fn fault_counters(&self) -> fightbox_runtime::FaultCounters {
        match self {
            Self::Binaural(graph) => graph.fault_counters(),
            Self::Ambix(graph) => graph.fault_counters(),
        }
    }

    fn safety_telemetry(&self) -> SafetyTelemetry {
        match self {
            Self::Binaural(graph) => graph.safety_telemetry(),
            Self::Ambix(graph) => graph.safety_telemetry(),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct TraceControl {
    recording: bool,
    generation: u64,
    ui_config_epoch: u64,
}

#[derive(Clone, Copy, PartialEq)]
struct TraceUiConfig {
    monitor_gain_db: f32,
    stages: StageMix,
    route: Option<SourceComparison>,
    source_heights: [SourceHeight; fightbox_runtime::MAX_ACTIVE_SOURCES],
}

struct WorkbenchTraceTap {
    writer: crate::level_trace::LevelTraceWriter,
    control_reader: SnapshotReader<TraceControl>,
    playback_reader: SnapshotReader<PlaybackSnapshot>,
    last_mix: SourceMix,
    last_config_epoch: u64,
    epoch: u64,
}

impl WorkbenchTraceTap {
    fn observe(&mut self, left: &[f32], right: &[f32], listener: EnuVector3) {
        let control = self.control_reader.read();
        let playback = self.playback_reader.read();
        // Mix adoption is witnessed in WorkbenchInput. Graph controls are only
        // UI-boundary observations, not sample-exact graph adoption claims.
        if playback.consumed_mix != self.last_mix
            || control.ui_config_epoch != self.last_config_epoch
        {
            self.epoch = self.epoch.wrapping_add(1);
            self.last_mix = playback.consumed_mix;
            self.last_config_epoch = control.ui_config_epoch;
        }
        self.writer.observe(
            left,
            right,
            crate::level_trace::BlockContext {
                recording: control.recording,
                identity: crate::level_trace::TraceIdentity {
                    generation: control.generation,
                    epoch: self.epoch,
                },
                listener_position_m: [listener.east_m, listener.north_m, listener.up_m],
            },
        );
    }
}

impl Drop for WorkbenchTraceTap {
    fn drop(&mut self) {
        self.writer.flush();
    }
}

struct LateBoundProcessor<P> {
    processor: P,
    pose_reader: SnapshotReader<ListenerState>,
    meter_writer: SnapshotWriter<MeterReading>,
    meter: MeterAccumulator,
    audio_block_writer: SnapshotWriter<u64>,
    capture_tap: Option<CaptureTap>,
    quiet_guard: Option<QuietOutputGuard>,
    elapsed_blocks: u64,
    level_trace: Option<WorkbenchTraceTap>,
    scene_gate: bool,
    gate_gain: f32,
}

// About 5 ms at 48 kHz. The gate runs after the limiter, where a hard cut
// clicks and its reconstruction can overshoot the true-peak ceiling.
const SCENE_GATE_FADE_STEP: f32 = 1.0 / 256.0;

impl<P> LateBoundProcessor<P> {
    fn new(
        processor: P,
        pose_reader: SnapshotReader<ListenerState>,
        meter_writer: SnapshotWriter<MeterReading>,
        meter: MeterAccumulator,
        audio_block_writer: SnapshotWriter<u64>,
        capture_tap: Option<CaptureTap>,
        quiet_guard: Option<QuietOutputGuard>,
    ) -> Self {
        Self {
            processor,
            pose_reader,
            meter_writer,
            meter,
            audio_block_writer,
            capture_tap,
            quiet_guard,
            elapsed_blocks: 0,
            level_trace: None,
            scene_gate: false,
            gate_gain: 1.0,
        }
    }
}

impl<P: ListenerStateSink> LateBoundProcessor<P> {
    fn finish_block(
        &mut self,
        result: Result<(), RenderError>,
        output_left: &mut [f32],
        output_right: &mut [f32],
        listener: ListenerState,
    ) -> Result<(), RenderError> {
        let gated = self.scene_gate && self.level_trace.as_mut().is_some_and(|trace| {
            trace.playback_reader.read().scene.is_none_or(|scene| !scene.running)
        });
        let target = if gated { 0.0 } else { 1.0 };
        if self.gate_gain == target {
            if gated {
                output_left.fill(0.0);
                output_right.fill(0.0);
            }
        } else {
            for frame in 0..output_left.len().max(output_right.len()) {
                self.gate_gain = if gated {
                    (self.gate_gain - SCENE_GATE_FADE_STEP).max(0.0)
                } else {
                    (self.gate_gain + SCENE_GATE_FADE_STEP).min(1.0)
                };
                if let Some(sample) = output_left.get_mut(frame) {
                    *sample *= self.gate_gain;
                }
                if let Some(sample) = output_right.get_mut(frame) {
                    *sample *= self.gate_gain;
                }
            }
        }
        if let Some(guard) = &mut self.quiet_guard {
            if result.is_err() {
                // Count faulted audio frames too; never preserve partial failed output.
                output_left.fill(0.0);
                output_right.fill(0.0);
            }
            guard.process_stereo(output_left, output_right);
        }
        if let Some(trace) = &mut self.level_trace {
            if result.is_ok() {
                trace.observe(output_left, output_right, listener.pose.position);
            } else {
                trace
                    .writer
                    .skip_frames(output_left.len().max(output_right.len()) as u64);
            }
        }
        result?;
        self.processor.capture_spatial_block(!gated);
        let reading = self.meter.observe(output_left, output_right);
        self.meter_writer.publish(reading);
        if let Some(capture_tap) = &self.capture_tap {
            capture_tap.capture_block(output_left, output_right);
        }
        self.elapsed_blocks = self.elapsed_blocks.saturating_add(1);
        self.audio_block_writer.publish(self.elapsed_blocks);
        Ok(())
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
        let result = self.processor.process_block(ProcessBlock {
            now_ns,
            sources,
            output_left: &mut *output_left,
            output_right: &mut *output_right,
        });
        self.finish_block(result, output_left, output_right, listener)
    }

    fn process_program_block(
        &mut self,
        block: fightbox_runtime::ProgramProcessBlock<'_>,
    ) -> Result<(), RenderError> {
        let listener = self.pose_reader.read();
        self.processor.set_listener_state(listener);
        let fightbox_runtime::ProgramProcessBlock {
            now_ns,
            sources,
            output_left,
            output_right,
        } = block;
        let result = self
            .processor
            .process_program_block(fightbox_runtime::ProgramProcessBlock {
                now_ns,
                sources,
                output_left: &mut *output_left,
                output_right: &mut *output_right,
            });
        self.finish_block(result, output_left, output_right, listener)
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
    /// Monotonic deque of `(absolute block index, block peak)` with strictly
    /// decreasing peaks from front to back. The front is therefore the maximum
    /// peak over exactly the blocks still inside the rolling window, which is
    /// what `observe` used to recover by folding all `blocks` entries.
    /// Pre-allocated to the window length; entries expire from the front as
    /// the window slides, and at most one entry is pushed per block, so the
    /// push can never grow the allocation (callback stays allocation-free).
    peak_frontier: VecDeque<(u64, f32)>,
    next_block_index: u64,
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
            peak_frontier: VecDeque::with_capacity(block_count.max(1)),
            next_block_index: 0,
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

        // Running maximum over the sliding window of block peaks. After this
        // observe, the window covers absolute indices
        // `[next_block_index - window, next_block_index - 1]`; evict anything
        // that slid out, then drop back entries the new peak dominates.
        let index = self.next_block_index;
        self.next_block_index += 1;
        let window = self.blocks.len() as u64;
        while self
            .peak_frontier
            .front()
            .is_some_and(|&(oldest, _)| oldest + window <= index)
        {
            self.peak_frontier.pop_front();
        }
        while self
            .peak_frontier
            .back()
            .is_some_and(|&(_, peak)| peak <= incoming.peak)
        {
            self.peak_frontier.pop_back();
        }
        self.peak_frontier.push_back((index, incoming.peak));

        // Identical to folding every ring slot before the window fills: the
        // never-written slots hold a 0.0 peak, which never exceeds the true
        // maximum because block peaks are absolute values (always >= 0.0).
        let peak = self.peak_frontier.front().map_or(0.0, |&(_, peak)| peak);
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
    OneShot,
    PeriodicOneShot { interval_frames: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourcePlayback {
    mode: PlaybackMode,
    consumed_generation: u64,
    cursor: usize,
    restart_cursor: usize,
    was_enabled: bool,
    restart_on_enable: bool,
    /// Silent frames before a retriggered program starts.
    start_delay_frames: u32,
    /// Only gun companions share an activation delay with a looping muzzle.
    honor_loop_delay: bool,
    /// Shots started so far; a non-looping source's echo trigger generation.
    shots: u64,
    clock: PlaybackClock,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PlaybackClock {
    audio_sample: u64,
    trigger_audio_sample: u64,
    event_sequence: u64,
    event_delay_frames: u32,
    enabled: bool,
    pending_retrigger: bool,
    trigger_offset_frames: u32,
    shots: u64,
    awaiting_first_shot: bool,
}

impl SourcePlayback {
    fn for_asset(
        asset_id: &str,
        sample_rate: u32,
        start_offset_s: f64,
        signal_frames: usize,
        restart_on_enable: bool,
        loops: bool,
    ) -> Self {
        debug_assert!(signal_frames > 0);
        let mode = if asset_id == ARTILLERY_ASSET_ID {
            PlaybackMode::PeriodicOneShot {
                interval_frames: artillery_retrigger_frames(sample_rate),
            }
        } else if loops {
            PlaybackMode::Looping
        } else {
            PlaybackMode::OneShot
        };
        let cursor = match mode {
            PlaybackMode::Looping => {
                ((start_offset_s * f64::from(sample_rate)).round() as usize) % signal_frames
            }
            PlaybackMode::OneShot | PlaybackMode::PeriodicOneShot { .. } => 0,
        };
        Self {
            mode,
            consumed_generation: 0,
            cursor,
            restart_cursor: cursor,
            was_enabled: false,
            restart_on_enable,
            start_delay_frames: 0,
            honor_loop_delay: false,
            shots: 0,
            clock: PlaybackClock::default(),
        }
    }

    #[cfg(test)]
    fn consume_retrigger(&mut self, generation: u64) {
        self.consume_retrigger_after(generation, 0);
    }

    fn is_one_shot(&self) -> bool {
        !matches!(self.mode, PlaybackMode::Looping)
    }

    /// Latest-value controls may skip intermediate generations. Consume the
    /// newest request once; never depend on observing an off/on UI pair.
    /// A non-looping restart first plays `start_delay_frames` of silence,
    /// counted from this block's first frame.
    fn consume_retrigger_after(&mut self, generation: u64, start_delay_frames: u32) {
        if generation != self.consumed_generation {
            self.consumed_generation = generation;
            self.clock.pending_retrigger = true;
            self.clock.event_delay_frames = start_delay_frames;
            self.clock.trigger_offset_frames = 0;
            if !matches!(self.mode, PlaybackMode::Looping) || (self.honor_loop_delay && start_delay_frames > 0) {
                self.cursor = self.restart_cursor;
                self.was_enabled = false;
                self.start_delay_frames = start_delay_frames;
            }
        }
    }

    fn rewind_scene(&mut self) {
        self.cursor = self.restart_cursor;
        self.was_enabled = false;
        self.start_delay_frames = 0;
    }

    fn consume_scene_retrigger(&mut self, generation: u64, delay_frames: u32, offset: u32) {
        self.rewind_scene();
        self.consumed_generation = generation;
        self.start_delay_frames = delay_frames;
        self.clock.pending_retrigger = true;
        self.clock.event_delay_frames = delay_frames;
        self.clock.trigger_offset_frames = offset;
    }

    fn status(&mut self, enabled: bool, signal_frames: usize) -> SourcePlaybackStatus {
        // Extend the existing block publication, keeping all timestamp work out of the sample loop.
        let started = self.clock.pending_retrigger || (enabled && !self.clock.enabled);
        if !enabled && !self.clock.pending_retrigger {
            self.clock.event_delay_frames = 0;
        }
        let periodic = enabled
            && !started
            && !self.clock.awaiting_first_shot
            && self.shots != self.clock.shots
            && matches!(self.mode, PlaybackMode::PeriodicOneShot { .. });
        if started || periodic {
            self.clock.trigger_audio_sample = if periodic {
                self.clock.audio_sample + u64::from(BLOCK_SIZE) - self.cursor as u64
            } else {
                self.clock.audio_sample + u64::from(self.clock.trigger_offset_frames)
            };
            self.clock.event_sequence = self.clock.event_sequence.wrapping_add(1);
            if periodic {
                self.clock.event_delay_frames = 0;
            }
            self.clock.pending_retrigger = false;
            if started {
                self.clock.awaiting_first_shot = self.is_one_shot();
            }
        }
        if self.shots != self.clock.shots {
            self.clock.awaiting_first_shot = false;
        }
        self.clock.enabled = enabled;
        self.clock.shots = self.shots;
        self.clock.audio_sample += u64::from(BLOCK_SIZE);
        SourcePlaybackStatus {
            generation: self.consumed_generation,
            observed: true,
            enabled,
            ended: enabled && self.mode == PlaybackMode::OneShot && self.cursor >= signal_frames,
            cursor: self.cursor as u64,
            audio_sample: self.clock.audio_sample,
            trigger_audio_sample: self.clock.trigger_audio_sample,
            event_sequence: self.clock.event_sequence,
            event_delay_frames: self.clock.event_delay_frames,
        }
    }

    fn next_sample(&mut self, signal: &[f32], enabled: bool) -> f32 {
        debug_assert!(!signal.is_empty());
        match self.mode {
            PlaybackMode::Looping => {
                if self.restart_on_enable {
                    if !enabled {
                        self.cursor = self.restart_cursor;
                        self.was_enabled = false;
                        self.start_delay_frames = 0;
                        return 0.0;
                    }
                    if !self.was_enabled {
                        self.cursor = self.restart_cursor;
                        self.was_enabled = true;
                    }
                }
                if self.honor_loop_delay && self.start_delay_frames > 0 {
                    self.start_delay_frames -= 1;
                    return 0.0;
                }
                if self.honor_loop_delay && self.cursor == self.restart_cursor {
                    self.shots += 1;
                }
                let sample = signal[self.cursor];
                self.cursor = (self.cursor + 1) % signal.len();
                sample
            }
            PlaybackMode::OneShot => {
                if !enabled {
                    self.cursor = 0;
                    self.was_enabled = false;
                    self.start_delay_frames = 0;
                    return 0.0;
                }
                if !self.was_enabled {
                    self.cursor = self.restart_cursor;
                    self.was_enabled = true;
                }
                if self.start_delay_frames > 0 {
                    self.start_delay_frames -= 1;
                    return 0.0;
                }
                // The shot starts on its first played sample, after any delay.
                if self.cursor == self.restart_cursor {
                    self.shots += 1;
                }
                let sample = signal.get(self.cursor).copied().unwrap_or(0.0);
                self.cursor = self.cursor.saturating_add(1);
                sample
            }
            PlaybackMode::PeriodicOneShot { interval_frames } => {
                if !enabled {
                    self.cursor = 0;
                    self.was_enabled = false;
                    self.start_delay_frames = 0;
                    return 0.0;
                }
                if !self.was_enabled {
                    self.cursor = 0;
                    self.was_enabled = true;
                }
                if self.start_delay_frames > 0 {
                    self.start_delay_frames -= 1;
                    return 0.0;
                }
                if self.cursor == 0 {
                    self.shots += 1;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SourcePlaybackStatus {
    generation: u64,
    observed: bool,
    enabled: bool,
    ended: bool,
    cursor: u64,
    audio_sample: u64,
    trigger_audio_sample: u64,
    event_sequence: u64,
    event_delay_frames: u32,
}

#[derive(Clone, Copy)]
struct PlaybackSnapshot {
    scene: Option<SceneStatus>,
    consumed_mix: SourceMix,
    sources: [SourcePlaybackStatus; fightbox_runtime::MAX_ACTIVE_SOURCES],
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            scene: None,
            consumed_mix: SourceMix::ALL_AUDIBLE,
            sources: [SourcePlaybackStatus::default(); fightbox_runtime::MAX_ACTIVE_SOURCES],
        }
    }
}

fn playback_status_label(status: SourcePlaybackStatus, requested_generation: u64) -> &'static str {
    if !status.observed || status.generation != requested_generation || !status.enabled {
        "Starting"
    } else if status.ended {
        "Shot finished · echoes may remain"
    } else {
        "Playing"
    }
}

#[cfg(feature = "live-output")]
struct WorkbenchInput {
    audio_sample: u64,
    signals: Vec<Vec<f32>>,
    program_plane_counts: Vec<usize>,
    song_readers: Vec<SongReader>,
    live_inputs: Vec<Option<crate::live_input::InputReader>>,
    live_mono: Vec<bool>,
    playback: Vec<SourcePlayback>,
    /// Crack companion slots, filled after every ordinary source.
    cracks: Vec<CrackPlayback>,
    scene: Option<SceneTimeline>,
    scene_control_reader: SnapshotReader<SceneControl>,
    scene_reset: Option<fightbox_steam_audio::SceneResetControl>,
    source_mix_reader: SnapshotReader<SourceMix>,
    playback_status_writer: SnapshotWriter<PlaybackSnapshot>,
    trace_playback_writer: SnapshotWriter<PlaybackSnapshot>,
    echo_trigger: Option<crate::echo_paths::ShotTrigger>,
}

/// Mono staging that [`WorkbenchInput`] fills: the device callback's buffer,
/// or fixed arrays in tests.
#[cfg(feature = "live-output")]
trait SourceBlockSink {
    fn add_source(&mut self, source_index: usize) -> Option<&mut [f32]>;

    fn add_program(
        &mut self,
        source_index: usize,
        count: usize,
    ) -> Option<fightbox_runtime::live::LiveSpatialProgramPlanes<'_>> {
        if count != 1 {
            return None;
        }
        self.add_source(source_index).map(|plane_zero| {
            fightbox_runtime::live::LiveSpatialProgramPlanes {
                plane_zero,
                plane_one: None,
            }
        })
    }
}

#[cfg(feature = "live-output")]
impl SourceBlockSink for fightbox_runtime::live::LiveSpatialSourceBuffer {
    fn add_source(&mut self, source_index: usize) -> Option<&mut [f32]> {
        self.add_source(source_index, 1)
            .map(|planes| planes.plane_zero)
    }

    fn add_program(
        &mut self,
        source_index: usize,
        count: usize,
    ) -> Option<fightbox_runtime::live::LiveSpatialProgramPlanes<'_>> {
        self.add_source(source_index, count)
    }
}

#[cfg(feature = "live-output")]
impl SourceBlockSink for fightbox_runtime::live::LiveSourceBuffer {
    fn add_source(&mut self, source_index: usize) -> Option<&mut [f32]> {
        fightbox_runtime::live::LiveSourceBuffer::add_source(self, source_index)
    }
}

#[cfg(feature = "live-output")]
impl fightbox_runtime::live::LiveInputProvider for WorkbenchInput {
    fn fill_block(&mut self, sources: &mut fightbox_runtime::live::LiveSourceBuffer) {
        self.fill_sources(sources);
    }
}

#[cfg(feature = "live-output")]
impl fightbox_runtime::live::LiveSpatialInputProvider for WorkbenchInput {
    fn fill_block(&mut self, sources: &mut fightbox_runtime::live::LiveSpatialSourceBuffer) {
        self.fill_sources(sources);
    }
}

#[cfg(feature = "live-output")]
impl WorkbenchInput {
    fn fill_sources(&mut self, sources: &mut impl SourceBlockSink) {
        let mut prepared_mix = self.source_mix_reader.read();
        let mut mix = prepared_mix;
        if let Some(scene) = &mut self.scene {
            let control = self.scene_control_reader.read();
            prepared_mix.retrigger_generations = control.prepared_generations;
            mix.retrigger_delay_frames = control.delay_frames;
            if scene.begin_block(control, self.audio_sample) {
                if let Some(reset) = &self.scene_reset { reset.reset(); }
                for playback in &mut self.playback {
                    playback.rewind_scene();
                    playback.clock.enabled = false;
                    playback.clock.pending_retrigger = false;
                }
                for crack in &mut self.cracks { crack.reset_scene(); }
            }
            // Enable is owned by the timeline; mute/solo/trim remain user controls.
            mix.enabled.fill(true);
        }
        let gains = mix.gains(self.signals.len());
        let mut status = PlaybackSnapshot {
            consumed_mix: mix,
            scene: self.scene.as_ref().map(|scene| scene.status),
            ..PlaybackSnapshot::default()
        };
        for index in 0..self.signals.len() {
            if let Some(song) = self.song_readers.get_mut(index) {
                if song.adopt_latest() {
                    if let Some(buffer) = song.buffer() {
                        self.playback[index] = SourcePlayback::for_asset("song", SAMPLE_RATE, 0.0, buffer.mono.len(), true, true);
                    }
                }
            }
            let song = self.song_readers.get(index).and_then(SongReader::buffer);
            let Some(planes) = sources.add_program(index, self.program_plane_counts[index]) else {
                return;
            };
            let output = planes.plane_zero;
            if song.is_none() && let Some(input) = &mut self.live_inputs[index] {
                let mut right = planes.plane_one;
                if self.live_mono.get(index).copied().unwrap_or(false) {
                    input.fill_block(output);
                    if let Some(right) = right.as_deref_mut() { right.copy_from_slice(output); }
                } else if let Some(right) = right.as_deref_mut() {
                    input.fill_stereo_block(output, right);
                } else {
                    input.fill_block(output);
                }
                for frame in 0..output.len() {
                    let enabled = self.scene.as_ref().map_or(mix.enabled[index], |scene| scene.block[index][frame].enabled);
                    if self.scene.as_ref().is_some_and(|scene| scene.block[index][frame].play) {
                        self.playback[index].clock.trigger_audio_sample = self.audio_sample + frame as u64;
                    }
                    let gain = gains[index] * f32::from(enabled);
                    output[frame] *= gain;
                    if let Some(right) = right.as_deref_mut() {
                        right[frame] *= gain;
                    }
                }
                let enabled = self.scene.as_ref().map_or(mix.enabled[index], |scene| scene.block[index][BLOCK_SIZE as usize - 1].enabled);
                status.consumed_mix.enabled[index] = enabled;
                status.sources[index] = SourcePlaybackStatus {
                    generation: mix.retrigger_generations[index],
                    observed: true, enabled,
                    audio_sample: self.audio_sample + u64::from(BLOCK_SIZE),
                    trigger_audio_sample: self.playback[index].clock.trigger_audio_sample,
                    ..SourcePlaybackStatus::default()
                };
                continue;
            }
            let signal = song.map_or(self.signals[index].as_slice(), |buffer| buffer.mono.as_slice());
            let mut right = planes.plane_one;
            let song_ready = song.is_none_or(|buffer| newer_play(prepared_mix.retrigger_generations[index], buffer.loaded_generation));
            if self.scene.is_none() {
                self.playback[index].consume_retrigger_after(
                    mix.retrigger_generations[index], mix.retrigger_delay_frames[index],
                );
            }
            let mut enabled = mix.enabled[index];
            for (frame, sample) in output.iter_mut().enumerate() {
                if let Some(scene) = &self.scene {
                    let cue = scene.block[index][frame];
                    enabled = cue.enabled;
                    if cue.play {
                        self.playback[index].consume_scene_retrigger(
                            cue.generation, mix.retrigger_delay_frames[index], frame as u32,
                        );
                        if !self.playback[index].is_one_shot() && !self.playback[index].honor_loop_delay && let Some(echo) = &self.echo_trigger {
                            echo.shot_started(index, cue.generation);
                        }
                    }
                    if !enabled {
                        self.playback[index].rewind_scene();
                        *sample = 0.0;
                        if let Some(right) = right.as_deref_mut() { right[frame] = 0.0; }
                        continue;
                    }
                }
                enabled &= song_ready;
                let shots = self.playback[index].shots;
                let cursor = self.playback[index].cursor;
                let mono = self.playback[index].next_sample(signal, enabled);
                let pair = song.and_then(|buffer| buffer.stereo.as_ref()).and_then(|frames| frames.get(cursor).copied());
                let gain = gains[index] * f32::from(enabled);
                *sample = pair.map_or(mono, |pair| pair[0]) * gain;
                if let Some(right) = right.as_deref_mut() {
                    right[frame] = pair.map_or(mono, |pair| pair[1]) * gain;
                }
                // Freeze echoes on the delayed impact's first played sample.
                if self.playback[index].shots != shots && let Some(echo) = &self.echo_trigger {
                    echo.shot_started(index, self.playback[index].shots);
                }
            }
            status.sources[index] = self.playback[index].status(enabled, signal.len());
            status.consumed_mix.enabled[index] = enabled;
            status.consumed_mix.retrigger_generations[index] = status.sources[index].generation;
        }
        for crack in &mut self.cracks {
            let Some(output) = sources.add_source(crack.slot_index) else { break; };
            if let Some(scene) = &self.scene {
                for (frame, sample) in output.iter_mut().enumerate() {
                    let cue = scene.block[crack.parent_index][frame];
                    crack.fill_scene(
                        prepared_mix.retrigger_generations[crack.parent_index],
                        cue.play, cue.enabled, gains[crack.parent_index], std::slice::from_mut(sample),
                    );
                }
            } else {
                crack.fill_enabled(mix.retrigger_generations[crack.parent_index], mix.enabled[crack.parent_index], gains[crack.parent_index], output);
            }
        }
        self.playback_status_writer.publish(status);
        self.trace_playback_writer.publish(status);
        self.audio_sample += u64::from(BLOCK_SIZE);
    }
}

#[cfg(feature = "live-output")]
struct EmptyInput;

#[cfg(feature = "live-output")]
impl fightbox_runtime::live::LiveInputProvider for EmptyInput {
    fn fill_block(&mut self, _sources: &mut fightbox_runtime::live::LiveSourceBuffer) {}
}

#[cfg(feature = "live-output")]
fn start_audio<P: BlockProcessor + Send + 'static>(
    processor: P,
    config: EngineConfig,
    signals: Vec<Vec<f32>>,
    live_devices: Vec<Option<String>>,
    live_mono: Vec<bool>,
    program_plane_counts: Vec<usize>,
    song_readers: Vec<SongReader>,
    live_input_wav: Option<PathBuf>,
    playback: Vec<SourcePlayback>,
    cracks: Vec<CrackPlayback>,
    scene: Option<SceneTimeline>,
    scene_control_reader: SnapshotReader<SceneControl>,
    scene_reset: Option<fightbox_steam_audio::SceneResetControl>,
    source_mix_reader: SnapshotReader<SourceMix>,
    playback_status_writer: SnapshotWriter<PlaybackSnapshot>,
    trace_playback_writer: SnapshotWriter<PlaybackSnapshot>,
    callback_timing_writer: CallbackTimingWriter,
    echo_trigger: Option<crate::echo_paths::ShotTrigger>,
    device: Option<&str>,
    require_exact_device: bool,
    null_output: bool,
    null_frame_limit: Option<u64>,
) -> AudioState {
    let prepared = match crate::live_input::prepare(
        &live_devices,
        live_input_wav.as_deref(),
        config.sample_rate_hz,
        device,
        null_output,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("{error}");
            return AudioState::Unavailable(error);
        }
    };
    let input = Box::new(WorkbenchInput {
        program_plane_counts,
        song_readers,
        audio_sample: 0,
        live_inputs: prepared.readers,
        live_mono,
        signals,
        playback,
        cracks,
        scene,
        scene_control_reader,
        scene_reset,
        source_mix_reader,
        playback_status_writer,
        trace_playback_writer,
        echo_trigger,
    });
    let processor = fightbox_runtime::live::ProgramInputProcessor::new(processor, input);
    let resolved_device = prepared.output_device.as_deref().or(device);
    // The program processor owns the input; streams receive an empty provider.
    let output = if null_output {
        fightbox_runtime::live::LiveOutput::new_null_with_input_and_timing_limit(
            processor,
            config,
            Box::new(EmptyInput),
            callback_timing_writer,
            null_frame_limit,
        )
    } else {
        match resolved_device {
            Some(name) => fightbox_runtime::live::LiveOutput::new_named_with_input_and_timing(
                processor,
                config,
                name,
                Box::new(EmptyInput),
                callback_timing_writer,
            ),
            None => fightbox_runtime::live::LiveOutput::new_default_with_input_and_timing(
                processor,
                config,
                Box::new(EmptyInput),
                callback_timing_writer,
            ),
        }
    };
    match output {
        Ok(output)
            if require_exact_device && !null_output && device != Some(output.device_name()) =>
        {
            AudioState::Unavailable(format!(
                "exact device mismatch before stream start: requested {device:?}, actual {:?}",
                output.device_name()
            ))
        }
        Ok(output) => match output.start() {
            Ok(()) => AudioState::Live(crate::live_input::LiveAudio::new(
                output,
                prepared.lifetimes,
                prepared.telemetry,
                prepared.controls,
            )),
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
    _live_devices: Vec<Option<String>>,
    _live_mono: Vec<bool>,
    _program_plane_counts: Vec<usize>,
    _song_readers: Vec<SongReader>,
    _live_input_wav: Option<PathBuf>,
    _playback: Vec<SourcePlayback>,
    _cracks: Vec<CrackPlayback>,
    _scene: Option<SceneTimeline>,
    _scene_control_reader: SnapshotReader<SceneControl>,
    _scene_reset: Option<fightbox_steam_audio::SceneResetControl>,
    _source_mix_reader: SnapshotReader<SourceMix>,
    _playback_status_writer: SnapshotWriter<PlaybackSnapshot>,
    _trace_playback_writer: SnapshotWriter<PlaybackSnapshot>,
    _callback_timing_writer: CallbackTimingWriter,
    _echo_trigger: Option<crate::echo_paths::ShotTrigger>,
    _device: Option<&str>,
    _require_exact_device: bool,
    _null_output: bool,
    _null_frame_limit: Option<u64>,
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

fn project_face(
    mesh: &AcousticMesh,
    face: MeshFace,
    eye: [f32; 3],
    fill: Color32,
    rect: Rect,
    camera_point: impl Fn(EnuVector3) -> [f32; 3],
    screen_point: impl Fn([f32; 3], Rect) -> Pos2,
) -> Option<ProjectedFace> {
    let world = face.indices.map(|index| mesh.vertices_enu_m[index]);
    // Backface culling by winding sign. The shading normal is
    // `normalize(cross(b - a, c - a))`, so a face whose normal points away
    // from the eye (positive dot with the eye->centroid vector) shows its
    // back side. The dot product is rotation-invariant, so the test runs on
    // world-space vectors and works for every camera basis. Edge-on faces
    // (dot == 0) are kept; they project to a degenerate sliver either way.
    let center = [
        (world[0].east_m + world[1].east_m + world[2].east_m) / 3.0,
        (world[0].north_m + world[1].north_m + world[2].north_m) / 3.0,
        (world[0].up_m + world[1].up_m + world[2].up_m) / 3.0,
    ];
    if dot3(face.normal, sub3(center, eye)) > 0.0 {
        return None;
    }
    let camera_points = world.map(camera_point);
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
        fill,
    })
}

/// True only when the projected polygon's bounding box lies entirely beyond
/// one boundary of `rect` expanded by `margin`. The near-clipped polygon of a
/// triangle is convex, so all vertices beyond the same boundary means the
/// whole face is outside the expanded rect, hence outside the clip rect:
/// dropping it cannot change any rendered pixel, margin or no margin.
fn projected_face_fully_outside(face: &ProjectedFace, rect: Rect, margin: f32) -> bool {
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for point in &face.points[..face.point_count] {
        min_x = min_x.min(point.x);
        max_x = max_x.max(point.x);
        min_y = min_y.min(point.y);
        max_y = max_y.max(point.y);
    }
    min_x > rect.right() + margin
        || max_x < rect.left() - margin
        || min_y > rect.bottom() + margin
        || max_y < rect.top() - margin
}

struct ProjectedFace {
    points: [Pos2; 4],
    point_count: usize,
    depth: f32,
    fill: Color32,
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

fn audition_map_rect(container: Rect) -> Rect {
    let margin = 12.0;
    let available_width = (container.width() - margin * 2.0).max(1.0);
    let available_height = (container.height() - margin * 2.0).max(1.0);
    let size = egui::vec2(
        (container.width() * 0.26)
            .clamp(210.0, 300.0)
            .min(available_width),
        (container.height() * 0.24)
            .clamp(140.0, 210.0)
            .min(available_height),
    );
    Rect::from_min_size(
        Pos2::new(
            container.right() - margin - size.x,
            container.top() + margin,
        ),
        size,
    )
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

fn anomaly_cell_color(cell: fightbox_steam_audio::ProxyCell, stale: bool) -> Color32 {
    let computation = [
        fightbox_steam_audio::AnomalyClass::InvalidEnergy,
        fightbox_steam_audio::AnomalyClass::InvalidCoefficient,
        fightbox_steam_audio::AnomalyClass::NeighborSpike,
        fightbox_steam_audio::AnomalyClass::ExcessiveDiscontinuity,
        fightbox_steam_audio::AnomalyClass::ZeroPathWithCoverage,
        fightbox_steam_audio::AnomalyClass::ReflectionEnergyExcess,
    ]
    .into_iter()
    .any(|class| cell.flags.contains(class));
    let alpha_scale = if stale { 0.35 } else { 1.0 };
    if computation {
        return Color32::from_rgba_unmultiplied(232, 72, 196, (170.0 * alpha_scale) as u8);
    }
    let risk = cell.score.clamp(0.0, 1.0);
    let red = (214.0 + 41.0 * risk) as u8;
    let green = (178.0 - 112.0 * risk) as u8;
    Color32::from_rgba_unmultiplied(red, green, 48, ((28.0 + risk * 120.0) * alpha_scale) as u8)
}

fn anomaly_ids(cell: fightbox_steam_audio::ProxyCell) -> String {
    let ids = fightbox_steam_audio::AnomalyClass::ALL
        .into_iter()
        .filter(|class| cell.flags.contains(*class))
        .map(fightbox_steam_audio::AnomalyClass::id)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        "no flags".into()
    } else {
        ids.join(", ")
    }
}

fn point_in_quad(point: Pos2, quad: [Pos2; 4]) -> bool {
    let mut sign = 0.0_f32;
    for index in 0..4 {
        let start = quad[index];
        let end = quad[(index + 1) % 4];
        let cross =
            (end.x - start.x) * (point.y - start.y) - (end.y - start.y) * (point.x - start.x);
        if cross.abs() <= f32::EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
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

fn build_ground_map(
    mesh: &AcousticMesh,
    bounds: Bounds2,
    street_lines: Vec<Vec<[f32; 2]>>,
) -> crate::ground_map::GroundMap {
    let mut walls = Vec::new();
    let mut roofs = Vec::new();
    for triangle in &mesh.triangles {
        let vertices = triangle.map(|index| mesh.vertices_enu_m[index as usize]);
        let [a, b, c] = vertices;
        let min_z = a.up_m.min(b.up_m).min(c.up_m);
        let max_z = a.up_m.max(b.up_m).max(c.up_m);
        if max_z - min_z < 0.1 && min_z > 0.5 {
            roofs.push(vertices.map(|point| [point.east_m, point.north_m]));
        }
        // Only vertical faces intersecting the ground-listener plane block this
        // sketch. Project their longest XY edge; discard degenerate poles.
        if max_z - min_z < 1.0 || min_z > 1.5 || max_z < 1.5 {
            continue;
        }
        let edge = [(a, b), (b, c), (c, a)]
            .into_iter()
            .max_by(|(a, b), (c, d)| {
                (a.east_m - b.east_m)
                    .hypot(a.north_m - b.north_m)
                    .total_cmp(&(c.east_m - d.east_m).hypot(c.north_m - d.north_m))
            })
            .unwrap();
        let (mut a, mut b) = (
            [edge.0.east_m, edge.0.north_m],
            [edge.1.east_m, edge.1.north_m],
        );
        if a == b {
            continue;
        }
        if a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])).is_gt() {
            std::mem::swap(&mut a, &mut b);
        }
        if !walls.contains(&(a, b)) {
            walls.push((a, b));
        }
    }
    crate::ground_map::GroundMap::new((bounds.min, bounds.max), 8.0, street_lines, walls, roofs)
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

    /// The ground point under a screen position, if that ray looks down.
    fn ground_point(self, screen: Pos2, rect: Rect, ground_up_m: f32) -> Option<[f32; 2]> {
        let aspect = rect.width() / rect.height().max(1.0);
        let x = (screen.x - rect.center().x) / (rect.width() * 0.5)
            * self.tan_half_vertical_fov
            * aspect;
        let y = -(screen.y - rect.center().y) / (rect.height() * 0.5) * self.tan_half_vertical_fov;
        if y >= -1.0e-4 {
            return None;
        }
        let depth = (ground_up_m - self.eye.up_m) / y;
        Some([
            self.eye.east_m + (self.right[0] * x + self.forward[0]) * depth,
            self.eye.north_m + (self.right[1] * x + self.forward[1]) * depth,
        ])
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

/// Sutherland-Hodgman against the near plane, for any convex polygon.
fn clip_to_near_plane(points: &[[f32; 3]], near_m: f32) -> Vec<[f32; 3]> {
    let mut clipped = Vec::with_capacity(points.len() + 2);
    let Some(&last) = points.last() else {
        return clipped;
    };
    let mut previous = last;
    for &current in points {
        let (previous_inside, current_inside) = (previous[2] >= near_m, current[2] >= near_m);
        if previous_inside != current_inside {
            clipped.push(if previous_inside {
                clip_to_depth(current, previous, near_m)
            } else {
                clip_to_depth(previous, current, near_m)
            });
        }
        if current_inside {
            clipped.push(current);
        }
        previous = current;
    }
    clipped
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

    /// Listener replay follows the open polyline once, then holds its end.
    fn sample_clamped_at_block(&self, elapsed_blocks: u64) -> (SourceTrajectorySample, bool) {
        let mut distance = elapsed_blocks as f64 * f64::from(BLOCK_SIZE) / f64::from(SAMPLE_RATE)
            * f64::from(self.speed_mps);
        for (index, length) in self
            .segment_lengths_m
            .iter()
            .copied()
            .take(self.waypoints.len() - 1)
            .enumerate()
        {
            if length <= 0.0 {
                continue;
            }
            if distance < f64::from(length) {
                let delta = subtract(self.waypoints[index + 1], self.waypoints[index]);
                return (
                    SourceTrajectorySample {
                        position: add(
                            self.waypoints[index],
                            scale(delta, distance as f32 / length),
                        ),
                        direction: scale(delta, 1.0 / length),
                    },
                    false,
                );
            }
            distance -= f64::from(length);
        }
        (
            SourceTrajectorySample {
                position: *self.waypoints.last().expect("validated nonempty route"),
                direction: EnuVector3::default(),
            },
            true,
        )
    }

    #[cfg(test)]
    fn sample_at_block(&self, elapsed_blocks: u64) -> SourceTrajectorySample {
        self.sample_at_frame(elapsed_blocks * u64::from(BLOCK_SIZE))
    }

    fn sample_at_frame(&self, elapsed_frames: u64) -> SourceTrajectorySample {
        let elapsed_seconds = elapsed_frames as f64 / f64::from(SAMPLE_RATE);
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

    #[cfg(feature = "live-output")]
    #[test]
    fn song_drop_never_starts_playback_and_callback_swap_allocates_nothing() {
        let (mut songs, song_reader) = song_channel();
        let mut mix = SourceMix::ALL_AUDIBLE;
        mix.retrigger_generations[0] = 5;
        let (mut mix_writer, mix_reader) = SnapshotPublication::new(mix);
        let (status_writer, mut status_reader) = SnapshotPublication::new(PlaybackSnapshot::default());
        let (trace_writer, _) = SnapshotPublication::new(PlaybackSnapshot::default());
        let mut input = WorkbenchInput {
            audio_sample: 0, signals: vec![vec![0.0]], program_plane_counts: vec![2],
            song_readers: vec![song_reader], live_inputs: vec![None], live_mono: vec![],
            playback: vec![SourcePlayback::for_asset("live", SAMPLE_RATE, 0.0, 1, true, true)],
            cracks: vec![], scene: None,
            scene_control_reader: SnapshotPublication::new(SceneControl::default()).1,
            scene_reset: None, source_mix_reader: mix_reader,
            playback_status_writer: status_writer, trace_playback_writer: trace_writer, echo_trigger: None,
        };
        let mut block = fightbox_runtime::live::LiveSpatialSourceBuffer::new(BLOCK_SIZE as usize);
        // Warm publications, then replace an already-enabled source. A stale
        // enabled control cannot start the newly adopted program.
        input.fill_sources(&mut block);
        songs.publish(SongBuffer { loaded_generation: 5, mono: vec![0.25; 64], stereo: Some(vec![[1.0, -0.5]; 64]) });
        block.clear();
        let calls = crate::ballistic_crack::tests::count_allocator_calls(|| input.fill_sources(&mut block));
        assert_eq!(calls, (0, 0));
        assert!(block.source_blocks()[0].program_planes.iter().flat_map(|plane| plane.iter()).all(|sample| *sample == 0.0));
        assert!(!status_reader.read().sources[0].enabled);
        mix.enabled[0] = false;
        mix_writer.publish(mix);
        block.clear(); input.fill_sources(&mut block);
        assert!(block.source_blocks()[0].program_planes.iter().flat_map(|plane| plane.iter()).all(|sample| *sample == 0.0));
        // The ordinary Play path supplies the newer generation.
        mix.enabled[0] = true; mix.retrigger_generations[0] = 6;
        mix_writer.publish(mix);
        block.clear(); input.fill_sources(&mut block);
        let planes = block.source_blocks();
        assert!(planes[0].program_planes[0].iter().all(|sample| *sample == 1.0));
        assert!(planes[0].program_planes[1].iter().all(|sample| *sample == -0.5));
        // A second drop during playback is silent too, and reclamation occurs
        // on the writer side after the callback has released the old bank.
        songs.publish(SongBuffer { loaded_generation: 6, mono: vec![0.5; 64], stereo: None });
        block.clear();
        assert_eq!(crate::ballistic_crack::tests::count_allocator_calls(|| input.fill_sources(&mut block)), (0, 0));
        assert!(block.source_blocks()[0].program_planes.iter().flat_map(|plane| plane.iter()).all(|sample| *sample == 0.0));
        songs.reclaim();
    }

    #[cfg(feature = "live-output")]
    #[test]
    fn stereo_live_program_stays_off_until_play_and_keeps_planes() {
        let (mut producer, consumer, _) = fightbox_runtime::live_input::stereo_ring(SAMPLE_RATE);
        for _ in 0..8_000 {
            producer.push([0.25, -0.5]);
        }
        let mut mix = SourceMix::ALL_AUDIBLE;
        mix.enabled[0] = false;
        let (mut writer, reader) = SnapshotPublication::new(mix);
        let (status_writer, _status_reader) = SnapshotPublication::new(PlaybackSnapshot::default());
        let (trace_writer, _trace_reader) = SnapshotPublication::new(PlaybackSnapshot::default());
        let (_scene_writer, scene_reader) = SnapshotPublication::new(SceneControl::default());
        let mut input = WorkbenchInput {
            audio_sample: 0,
            scene: None,
            scene_control_reader: scene_reader,
            scene_reset: None,
            signals: vec![vec![0.0]],
            song_readers: vec![],
            live_mono: vec![],
            program_plane_counts: vec![2],
            live_inputs: vec![Some(fightbox_runtime::live_input::AdaptiveInput::new(
                consumer,
                SAMPLE_RATE,
                SAMPLE_RATE,
            ).into())],
            // Production keeps one playback clock per source, live ones included.
            playback: vec![SourcePlayback::for_asset("live", SAMPLE_RATE, 0.0, 1, false, true)],
            cracks: vec![],
            source_mix_reader: reader,
            playback_status_writer: status_writer,
            trace_playback_writer: trace_writer,
            echo_trigger: None,
        };
        let mut sources = fightbox_runtime::live::LiveSpatialSourceBuffer::new(BLOCK_SIZE as usize);
        for _ in 0..3 {
            sources.clear();
            input.fill_sources(&mut sources);
            let blocks = sources.source_blocks();
            assert_eq!(blocks[0].program_plane_count, 2);
            assert!(
                blocks[0]
                    .program_planes
                    .iter()
                    .flat_map(|plane| plane.iter())
                    .all(|sample| *sample == 0.0)
            );
        }
        mix.enabled[0] = true;
        writer.publish(mix);
        sources.clear();
        input.fill_sources(&mut sources);
        let blocks = sources.source_blocks();
        assert!(
            blocks[0].program_planes[0]
                .iter()
                .all(|sample| *sample == 0.25)
        );
        assert!(
            blocks[0].program_planes[1]
                .iter()
                .all(|sample| *sample == -0.5)
        );
    }

    #[cfg(feature = "live-output")]
    #[test]
    fn scene_audio_input_is_sample_accurate_with_delay_retrigger_and_stop() {
        struct Sink([f32; BLOCK_SIZE as usize]);
        impl SourceBlockSink for Sink {
            fn add_source(&mut self, _: usize) -> Option<&mut [f32]> { Some(&mut self.0) }
        }
        let mut wire: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/city/astra-artillery/street-path-candidate.json"
        )).unwrap();
        wire["cues"] = serde_json::json!([
            {"at_s": 126.0 / 48_000.0, "play": "artillery-corner-shot"},
            {"at_s": 136.0 / 48_000.0, "stop": "artillery-corner-shot"},
            {"at_s": 258.0 / 48_000.0, "play": "artillery-corner-shot"},
            {"at_s": 263.0 / 48_000.0, "stop": "artillery-corner-shot"}
        ]);
        let fixture = Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "input-cues").unwrap();
        let (mut control_writer, control_reader) = SnapshotPublication::new(SceneControl::default());
        let (_, mix_reader) = SnapshotPublication::new(SourceMix::ALL_AUDIBLE);
        let (status_writer, mut status_reader) = SnapshotPublication::new(PlaybackSnapshot::default());
        let (trace_writer, _) = SnapshotPublication::new(PlaybackSnapshot::default());
        let mut input = WorkbenchInput {
            audio_sample: 0,
            signals: vec![vec![1.0, 0.5]], program_plane_counts: vec![1], song_readers: vec![], live_inputs: vec![None], live_mono: vec![],
            playback: vec![SourcePlayback::for_asset("finite", SAMPLE_RATE, 0.0, 2, false, false)],
            cracks: Vec::new(), scene: Some(SceneTimeline::new(&fixture, SAMPLE_RATE)),
            scene_control_reader: control_reader, scene_reset: None, source_mix_reader: mix_reader,
            playback_status_writer: status_writer, trace_playback_writer: trace_writer, echo_trigger: None,
        };
        let mut sink = Sink([0.0; BLOCK_SIZE as usize]);
        input.fill_sources(&mut sink);
        assert!(sink.0.iter().all(|sample| *sample == 0.0), "saved enabled controls cannot play before Play scene");
        let mut control = SceneControl { generation: 1, running: true, ..SceneControl::default() };
        control.delay_frames[0] = 4;
        control.prepared_generations[0] = 1;
        control_writer.publish(control);
        let mut pcm = Vec::new();
        for block in 0..3 {
            input.fill_sources(&mut sink);
            pcm.extend_from_slice(&sink.0);
            if block == 0 {
                let status = status_reader.read();
                assert_eq!(status.sources[0].trigger_audio_sample, 128 + 126);
                assert_eq!(status.sources[0].event_delay_frames, 4);
                assert_eq!(status.scene.unwrap().start_audio_sample, 128);
            }
        }
        assert_eq!(pcm.iter().enumerate().filter(|(_, sample)| **sample != 0.0).map(|(frame, sample)| (frame, *sample)).collect::<Vec<_>>(),
            vec![(130, 1.0), (131, 0.5), (262, 1.0)]);
        assert_eq!(status_reader.read().sources[0].event_sequence, 2, "V1 receives both cued one-shots");
        control.running = false;
        control_writer.publish(control);
        input.fill_sources(&mut sink);
        assert!(sink.0.iter().all(|sample| *sample == 0.0));
        assert_eq!(status_reader.read().scene.unwrap().frame, 0);
        control.running = true;
        control.generation += 1;
        control_writer.publish(control);
        input.fill_sources(&mut sink);
        assert_eq!(status_reader.read().sources[0].trigger_audio_sample, 640 + 126);
    }

    #[test]
    fn scene_slot_teardown_and_rebuild_never_leaks_previous_scene_ids() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let megablock = Fixture::read(&root.join("fixtures/city/megablock/fixture.json")).unwrap();
        let checkpoint = Fixture::read(&root.join("fixtures/checkpoint/fixture.json")).unwrap();

        let megablock_ids = planned_physical_source_ids(&megablock);
        let checkpoint_ids = planned_physical_source_ids(&checkpoint);
        assert_eq!(megablock_ids.len(), 5);
        // The checkpoint contract remains eight sources, now with capacity
        // headroom for the next content wave.
        assert_eq!(checkpoint_ids.len(), 8);
        assert!(checkpoint_ids.len() < fightbox_runtime::MAX_ACTIVE_SOURCES);
        assert!(megablock_ids.contains(&"dshk-street-gun".into()));
        assert!(checkpoint_ids.contains(&"m2-checkpoint-gun".into()));
        assert!(checkpoint_ids.contains(&"dshk-return-fire".into()));
        assert!(checkpoint_ids.contains(&"a10-gunrun-sky".into()));
        assert!(checkpoint_ids.contains(&"a10-strike-line".into()));
        assert!(checkpoint_ids.contains(&"a10-gunrun-sky-west".into()));
        assert!(checkpoint_ids.contains(&"a10-strike-line-west".into()));

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

    struct LoudProcessor;
    impl ListenerStateSink for LoudProcessor {
        fn set_listener_state(&mut self, _: ListenerState) {}
    }
    impl BlockProcessor for LoudProcessor {
        fn block_size_frames(&self) -> usize {
            128
        }
        fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
            block.output_left.fill(1.0);
            block.output_right.fill(-0.5);
            Ok(())
        }
    }

    #[test]
    fn measured_trace_taps_unchanged_output_with_held_pose_mix_boundaries_and_stop_flush() {
        let listener = ListenerControl::at(
            EnuVector3::new(4.0, 5.0, 1.5),
            EnuVector3::new(0.0, 1.0, 0.0),
        )
        .listener_state(EnuVector3::default());
        let (mut mailbox, pose_reader) = PoseMailbox::new(listener);
        let (meter_writer, _) = SnapshotPublication::new(MeterReading::SILENT);
        let (clock_writer, _) = SnapshotPublication::new(0_u64);
        let (mut control_writer, control_reader) = SnapshotPublication::new(TraceControl {
            recording: true,
            generation: 1,
            ui_config_epoch: 0,
        });
        let (mut playback_writer, playback_reader) =
            SnapshotPublication::new(PlaybackSnapshot::default());
        let (writer, mut reader) = crate::level_trace::channel(1000, 32, 32).unwrap();
        let mut processor = LateBoundProcessor::new(
            LoudProcessor,
            pose_reader,
            meter_writer,
            MeterAccumulator::new(1000, 128, 0.5),
            clock_writer,
            None,
            None,
        );
        processor.level_trace = Some(WorkbenchTraceTap {
            writer,
            control_reader,
            playback_reader,
            last_mix: SourceMix::ALL_AUDIBLE,
            last_config_epoch: 0,
            epoch: 0,
        });
        let mut left = [0.0; 128];
        let mut right = [0.0; 128];
        for block in 0..4 {
            if block == 1 {
                let mut changed = PlaybackSnapshot::default();
                changed.consumed_mix.retrigger_generations[0] = 7;
                playback_writer.publish(changed);
                let moved = ListenerControl::at(
                    EnuVector3::new(14.0, 5.0, 1.5),
                    EnuVector3::new(0.0, 1.0, 0.0),
                )
                .listener_state(EnuVector3::default());
                mailbox.publish(moved);
            }
            if block == 2 {
                control_writer.publish(TraceControl {
                    recording: false,
                    generation: 1,
                    ui_config_epoch: 0,
                });
            }
            if block == 3 {
                control_writer.publish(TraceControl {
                    recording: true,
                    generation: 2,
                    ui_config_epoch: 0,
                });
            }
            processor
                .process_block(ProcessBlock {
                    now_ns: block * 128_000_000,
                    sources: &[],
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
            assert_eq!(left, [1.0; 128], "trace must never alter final PCM");
            assert_eq!(right, [-0.5; 128]);
        }
        drop(processor); // remaining partial recording is retained on disposal.
        reader.drain();
        let samples = reader.samples();
        assert_eq!(samples.len(), 6);
        for sample in samples {
            assert!((sample.rms_linear.unwrap() - 0.625_f64.sqrt()).abs() < 1e-12);
            assert_eq!(sample.peak_linear, Some(1.0));
        }
        assert_eq!(samples[0].listener_start_m, [4.0, 5.0, 1.5]);
        assert_eq!((samples[1].start_frame, samples[1].end_frame), (100, 128));
        assert_eq!(
            samples[1].end_reason,
            crate::level_trace::WindowEnd::IdentityChanged
        );
        assert_ne!(samples[1].identity.epoch, samples[2].identity.epoch);
        assert_eq!(samples[2].listener_start_m, [14.0, 5.0, 1.5]);
        assert_eq!(
            samples[3].end_reason,
            crate::level_trace::WindowEnd::RecordingStopped
        );
        assert_eq!(
            (samples[4].start_frame, samples[4].identity.generation),
            (384, 2)
        );
        assert!(!samples[4].can_connect_from(&samples[3]));
        assert_eq!(
            samples[5].end_reason,
            crate::level_trace::WindowEnd::Flushed
        );
    }

    #[test]
    fn quiet_guard_bounds_actual_output_before_meter_and_default_route_is_unchanged() {
        for quiet in [false, true] {
            let listener = ListenerControl::at(
                EnuVector3::new(0.0, 0.0, 1.5),
                EnuVector3::new(0.0, 1.0, 0.0),
            )
            .listener_state(EnuVector3::default());
            let (_mailbox, reader) = PoseMailbox::new(listener);
            let (meter_writer, mut meter_reader) = SnapshotPublication::new(MeterReading::SILENT);
            let (clock_writer, _clock_reader) = SnapshotPublication::new(0_u64);
            let guard = quiet.then(|| QuietOutputGuard::new(1, 1000).unwrap());
            let status = guard.as_ref().map(QuietOutputGuard::reader);
            let mut processor = LateBoundProcessor::new(
                LoudProcessor,
                reader,
                meter_writer,
                MeterAccumulator::new(1000, 128, 0.5),
                clock_writer,
                None,
                guard,
            );
            let mut left = [0.0; 128];
            let mut right = [0.0; 128];
            for index in 0..9 {
                processor
                    .process_block(ProcessBlock {
                        now_ns: index * 128_000_000,
                        sources: &[],
                        output_left: &mut left,
                        output_right: &mut right,
                    })
                    .unwrap();
                if quiet {
                    assert!(
                        left.iter()
                            .chain(&right)
                            .all(|sample| sample.abs() <= 0.001_758)
                    );
                    assert!(meter_reader.read().peak_dbfs <= QUIET_CEILING_DBFS + 0.001);
                } else {
                    assert_eq!(left, [1.0; 128]);
                    assert_eq!(right, [-0.5; 128]);
                    assert_eq!(meter_reader.read().peak_dbfs, 0.0);
                }
            }
            if let Some(status) = status {
                assert!(status.read().expired);
                assert!(left.iter().chain(&right).all(|sample| *sample == 0.0));
            }
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
                non_finite_blocks: 0,
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
                non_finite_blocks: 0,
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
                        [0.0, 0.0, 1.5],
                        Color32::GRAY,
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
    fn restartable_default_off_source_ignores_saved_enabled_state_at_startup() {
        assert!(!startup_source_enabled(false, true, Some(true)));
        assert!(!startup_source_enabled(false, true, None));
        assert!(startup_source_enabled(false, false, Some(true)));
        assert!(!startup_source_enabled(true, true, Some(false)));
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
    fn quiet_start_consumes_once_and_rebuild_keeps_only_unspent_budget() {
        let mut guard = QuietOutputGuard::new(1, 1000).unwrap();
        let reader = guard.reader();
        let starts = std::rc::Rc::new(std::cell::Cell::new(0));
        let make_start = || {
            let starts = starts.clone();
            Some(Box::new(move || {
                starts.set(starts.get() + 1);
                AudioState::Stopped
            }) as Box<dyn FnOnce() -> AudioState>)
        };
        let mut pending = make_start();
        assert!(take_quiet_start(&mut pending, None).is_none());
        assert_eq!(starts.get(), 0);
        let start = take_quiet_start(&mut pending, Some(&reader)).unwrap();
        assert_eq!(
            starts.get(),
            0,
            "taking a closure alone must not open output"
        );
        start();
        assert_eq!(starts.get(), 1);
        assert!(take_quiet_start(&mut pending, Some(&reader)).is_none());
        guard.process_stereo(&mut [0.0; 400], &mut [0.0; 400]);
        let mut rebuilt_pending = make_start();
        take_quiet_start(&mut rebuilt_pending, Some(&reader)).unwrap()();
        assert_eq!(reader.read().processed_frames, 400);
        assert_eq!(starts.get(), 2);
        let mut rebuilt_guard = guard.clone();
        rebuilt_guard.process_stereo(&mut [0.0; 700], &mut [0.0; 700]);
        assert!(reader.read().expired);
        let mut expired_pending = make_start();
        assert!(take_quiet_start(&mut expired_pending, Some(&reader)).is_none());
        assert_eq!(
            starts.get(),
            2,
            "rebuild cannot restart an expired audition"
        );
    }

    #[test]
    fn authored_reflection_bypass_initializes_output_gains_and_capture_state() {
        let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../fixtures/city/chicago-walk/fixture.json"
        ))
        .unwrap();
        for enabled in [false, true] {
            value["simulation"]["reflections"]["enabled"] = enabled.into();
            let fixture =
                Fixture::parse(&serde_json::to_vec(&value).unwrap(), "authored-stages").unwrap();
            let mix = StageMix::from_fixture(&fixture);
            assert_eq!(
                mix.gains(),
                StageOutputGains {
                    direct: 1.0,
                    pathing: 1.0,
                    reflections: f32::from(enabled),
                }
            );
            let captured = crate::capture::CaptureStageState::from(mix);
            assert_eq!(captured.reflections.output_enabled, enabled);
            assert_eq!(captured.reflections.bypassed, !enabled);
            assert!(captured.direct.output_enabled && captured.pathing.output_enabled);
        }
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
    fn checkpoint_a10_racetrack_is_phase_locked_to_both_strike_streets() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let fixture = Fixture::read(&root.join("fixtures/checkpoint/fixture.json")).unwrap();
        let trajectory =
            SourceTrajectory::from_fixture(fixture.sources[4].trajectory.as_ref().unwrap())
                .unwrap();

        assert!((trajectory.cycle_length_m / trajectory.speed_mps - 84.0).abs() < 1.0e-4);

        let east_impact = trajectory.sample_at_block(38 * 375);
        assert!((east_impact.position.east_m - 292.5).abs() < 1.0e-3);
        assert!((east_impact.position.north_m - 312.025).abs() < 0.02);
        assert!(east_impact.direction.north_m > 0.999);

        let east_report = trajectory.sample_at_block(14_985);
        assert!((east_report.position.east_m - 292.5).abs() < 1.0e-3);
        assert!((east_report.position.north_m - 332.5).abs() < 0.02);

        let west_impact = trajectory.sample_at_block(80 * 375);
        assert!((west_impact.position.east_m - 197.5).abs() < 1.0e-3);
        assert!((west_impact.position.north_m - 372.975).abs() < 0.02);
        assert!(west_impact.direction.north_m < -0.999);

        let west_report = trajectory.sample_at_block(30_735);
        assert!((west_report.position.east_m - 197.5).abs() < 1.0e-3);
        assert!((west_report.position.north_m - 352.5).abs() < 0.02);
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
    fn audition_map_is_a_bounded_top_right_inset() {
        let main = Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(1010.0, 620.0));
        let inset = audition_map_rect(main);
        assert!(main.contains_rect(inset));
        assert!(inset.width() <= 300.0);
        assert!(inset.height() <= 210.0);
        assert!(inset.width() < main.width() * 0.35);
        assert!(inset.height() < main.height() * 0.35);
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
        let configured =
            SourcePlayback::for_asset(ARTILLERY_ASSET_ID, 48_000, 0.0, 1, false, false);
        assert_eq!(
            configured.mode,
            PlaybackMode::PeriodicOneShot {
                interval_frames: 144_000
            }
        );
        assert_eq!(
            SourcePlayback::for_asset("toms-diner", 48_000, 0.0, 1, false, true).mode,
            PlaybackMode::Looping
        );
        let mut playback = SourcePlayback {
            consumed_generation: 0,
            mode: PlaybackMode::PeriodicOneShot { interval_frames: 4 },
            cursor: 0,
            restart_cursor: 0,
            was_enabled: false,
            restart_on_enable: false,
            start_delay_frames: 0,
            honor_loop_delay: false,
            shots: 0,
            clock: PlaybackClock::default(),
        };
        let signal = [1.0, 0.5];
        let enabled = (0..6)
            .map(|_| playback.next_sample(&signal, true))
            .collect::<Vec<_>>();
        assert_eq!(enabled, vec![1.0, 0.5, 0.0, 0.0, 1.0, 0.5]);
        // Each period's first sample is a new shot for the echo trigger.
        assert_eq!(playback.shots, 2);
        assert_eq!(playback.next_sample(&signal, false), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
        assert_eq!(playback.shots, 3);
    }

    #[test]
    fn one_shot_counts_a_shot_on_each_start_and_retrigger_only() {
        let signal = [1.0, 0.5, 0.25];
        let mut playback =
            SourcePlayback::for_asset("astra-artillery-single", 48_000, 0.0, 3, true, false);
        assert!(playback.is_one_shot());
        assert_eq!(playback.next_sample(&signal, false), 0.0);
        assert_eq!(playback.shots, 0);
        for _ in 0..5 {
            playback.next_sample(&signal, true);
        }
        assert_eq!(playback.shots, 1);
        playback.consume_retrigger(1);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
        assert_eq!(playback.shots, 2);
        // A delayed restart counts its shot on the first played sample.
        playback.consume_retrigger_after(2, 2);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.shots, 2);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
        assert_eq!(playback.shots, 3);
        assert!(
            !SourcePlayback::for_asset("toms-diner", 48_000, 0.0, 1, false, true).is_one_shot()
        );
    }

    #[test]
    fn restartable_loop_is_silent_while_disabled_and_rewinds_on_enable() {
        let signal = [1.0, 2.0, 3.0];
        let mut playback =
            SourcePlayback::for_asset("audition", 48_000, 0.0, signal.len(), true, true);

        assert_eq!(playback.next_sample(&signal, false), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
        assert_eq!(playback.next_sample(&signal, true), 2.0);
        assert_eq!(playback.next_sample(&signal, false), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
    }

    #[test]
    fn finite_asset_plays_once_and_rearms_after_disable() {
        let signal = [1.0, 2.0];
        let mut playback =
            SourcePlayback::for_asset("finite", 48_000, 0.0, signal.len(), true, false);
        assert_eq!(playback.mode, PlaybackMode::OneShot);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
        assert_eq!(playback.next_sample(&signal, true), 2.0);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.next_sample(&signal, false), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
    }

    #[test]
    fn finite_listen_restarts_after_end_and_coalesced_requests_are_consumed_once() {
        let signal = [0.7, -0.4, 0.2];
        let mut playback =
            SourcePlayback::for_asset("finite", 48_000, 0.0, signal.len(), true, false);
        playback.consume_retrigger(1);
        for expected in signal {
            assert_eq!(playback.next_sample(&signal, true), expected);
        }
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(
            playback_status_label(playback.status(true, signal.len()), 1),
            "Shot finished · echoes may remain"
        );
        assert_eq!(
            playback_status_label(playback.status(true, signal.len()), 4),
            "Starting"
        );
        // Generations 2/3 may be superseded before the callback reads the snapshot.
        playback.consume_retrigger(4);
        assert_eq!(playback.next_sample(&signal, true), 0.7);
        assert_eq!(
            playback_status_label(playback.status(true, signal.len()), 4),
            "Playing"
        );
        playback.consume_retrigger(4);
        assert_eq!(
            playback.next_sample(&signal, true),
            -0.4,
            "same request must not restart every block"
        );
        playback.consume_retrigger(5);
        assert_eq!(
            playback.next_sample(&signal, true),
            0.7,
            "fresh request restarts while still enabled"
        );
    }

    #[test]
    fn listen_generation_does_not_reset_looping_phase() {
        let signal = [1.0, 2.0, 3.0];
        let mut playback = SourcePlayback::for_asset("loop", 48_000, 0.0, signal.len(), true, true);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
        playback.consume_retrigger(1);
        assert_eq!(playback.next_sample(&signal, true), 2.0);
        playback.consume_retrigger(9);
        assert_eq!(playback.next_sample(&signal, true), 3.0);
        assert!(!playback.status(true, signal.len()).ended);
    }

    #[test]
    fn delayed_retrigger_is_sample_accurate_and_cancelled_by_disable() {
        let signal = [0.7, -0.4];
        let mut playback =
            SourcePlayback::for_asset("finite", 48_000, 0.0, signal.len(), true, false);
        playback.consume_retrigger_after(1, 3);
        let rendered = (0..6)
            .map(|_| playback.next_sample(&signal, true))
            .collect::<Vec<_>>();
        assert_eq!(rendered, [0.0, 0.0, 0.0, 0.7, -0.4, 0.0]);
        playback.consume_retrigger_after(1, 3);
        assert_eq!(playback.next_sample(&signal, true), 0.0, "consumed once");

        playback.consume_retrigger_after(2, 3);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.next_sample(&signal, false), 0.0);
        // A plain re-enable without a new generation restarts undelayed.
        assert_eq!(playback.next_sample(&signal, true), 0.7);

        let mut looping = SourcePlayback::for_asset("loop", 48_000, 0.0, 2, true, true);
        looping.consume_retrigger_after(1, 5);
        assert_eq!(looping.next_sample(&signal, true), 0.7, "loops never wait");
    }

    #[cfg(feature = "live-output")]
    #[test]
    fn crack_and_delayed_impact_start_in_one_callback_with_exact_spacing() {
        struct Sink {
            blocks: [[f32; BLOCK_SIZE as usize]; 4],
            indices: [usize; 4],
            len: usize,
        }
        impl SourceBlockSink for Sink {
            fn add_source(&mut self, source_index: usize) -> Option<&mut [f32]> {
                let slot = self.len;
                if slot == self.blocks.len() {
                    return None;
                }
                self.len += 1;
                self.indices[slot] = source_index;
                self.blocks[slot].fill(f32::NAN);
                Some(&mut self.blocks[slot])
            }
        }

        let fixture = Fixture::parse(
            include_bytes!("../../../fixtures/city/astra-artillery/street-path-candidate.json"),
            "street-path-candidate.json",
        )
        .unwrap();
        let source = &fixture.sources[0];
        let spot_a = EnuVector3::new(434.02, 483.82, 1.5);
        // An impact whose physical onset follows 100 frames of silence.
        let mut impact = vec![0.0_f32; 4_000];
        impact[100] = 1.0;
        impact[101] = -0.5;
        let declaration = BallisticCrack::declare(
            0,
            1,
            source,
            source.ballistic.as_ref().unwrap(),
            &impact,
            SAMPLE_RATE,
            BLOCK_SIZE,
            spot_a,
        )
        .unwrap();
        let mut crack = declaration.crack;
        let shot = crack.arm(1, spot_a).unwrap();

        let mut mix = SourceMix::ALL_AUDIBLE;
        mix.enabled[0] = false;
        let (mut mix_writer, mix_reader) = SnapshotPublication::new(mix);
        let (status_writer, mut status_reader) =
            SnapshotPublication::new(PlaybackSnapshot::default());
        let (trace_writer, _trace_reader) = SnapshotPublication::new(PlaybackSnapshot::default());
        let mut input = WorkbenchInput {
            audio_sample: 0,
            signals: vec![impact.clone()],
            song_readers: vec![],
            live_mono: vec![],
            program_plane_counts: vec![1],
            live_inputs: vec![None],
            playback: vec![SourcePlayback::for_asset(
                source.asset_id.as_str(),
                SAMPLE_RATE,
                0.0,
                impact.len(),
                true,
                false,
            )],
            cracks: vec![declaration.playback],
            scene: None,
            scene_control_reader: SnapshotPublication::new(SceneControl::default()).1,
            scene_reset: None,
            source_mix_reader: mix_reader,
            playback_status_writer: status_writer,
            trace_playback_writer: trace_writer,
            echo_trigger: None,
        };
        let mut sink = Sink {
            blocks: [[0.0; BLOCK_SIZE as usize]; 4],
            indices: [usize::MAX; 4],
            len: 0,
        };
        input.fill_sources(&mut sink);
        assert_eq!((sink.len, &sink.indices[..2]), (2, &[0, 1][..]));
        assert!(
            sink.blocks[..2]
                .iter()
                .flatten()
                .all(|sample| *sample == 0.0)
        );

        // The Listen press: one mix carries enable, solo, generation, delay.
        mix.enabled[0] = true;
        mix.soloed[0] = true;
        mix.retrigger_generations[0] = 1;
        mix.retrigger_delay_frames[0] = shot.impact_delay_frames;
        mix_writer.publish(mix);
        let (mut impact_out, mut crack_out) = (Vec::new(), Vec::new());
        let mut echo_trigger_block = None;
        for block in 0..600 {
            sink.len = 0;
            let shots = input.playback[0].shots;
            input.fill_sources(&mut sink);
            if block == 0 {
                assert_eq!(status_reader.read().sources[0].generation, 1);
            }
            if input.playback[0].shots != shots {
                assert_eq!(echo_trigger_block, None, "one echo trigger per shot");
                echo_trigger_block = Some(block);
            }
            impact_out.extend_from_slice(&sink.blocks[0]);
            crack_out.extend_from_slice(&sink.blocks[1]);
        }
        // The echo trigger rides the delayed impact, not the press callback.
        assert_eq!(
            echo_trigger_block,
            Some(shot.impact_delay_frames as usize / BLOCK_SIZE as usize)
        );
        let first_sound = |samples: &[f32]| samples.iter().position(|sample| *sample != 0.0);
        let crack_onset = first_sound(&crack_out).unwrap();
        let impact_onset = first_sound(&impact_out).unwrap();
        // Both programs count from the first frame of the press callback.
        let pre_roll = (crate::ballistic_crack::CRACK_PRE_ROLL_SECONDS * f64::from(SAMPLE_RATE))
            .round() as usize;
        assert_eq!(crack_onset, pre_roll);
        assert_eq!(impact_onset, shot.impact_delay_frames as usize + 100);
        let t_star_s = shot.plan.tangent.unwrap().emission_time_s;
        let emission_to_end_s = shot.plan.trajectory_end.projectile_time_s - t_star_s;
        let expected_spacing = ((emission_to_end_s
            + crate::ballistic_crack::CRACK_PRE_ROLL_SECONDS)
            * f64::from(SAMPLE_RATE))
        .round() as usize
            - pre_roll;
        assert_eq!(impact_onset - crack_onset, expected_spacing);
        assert!(
            ((impact_onset - crack_onset) as f64 / f64::from(SAMPLE_RATE) - 1.233_776).abs()
                < 1.0e-3
        );
    }

    #[test]
    fn looping_playback_start_offset_selects_an_exact_sample_phase() {
        let signal = [0.0, 1.0, 2.0, 3.0];
        let mut playback =
            SourcePlayback::for_asset("squad-a10-pass", 2, 1.5, signal.len(), false, true);

        assert_eq!(playback.next_sample(&signal, true), 3.0);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
    }

    #[test]
    fn gun_loop_pre_roll_is_once_per_play_and_retrigger_is_sample_synchronous() {
        let signal = [1.0, 2.0, 3.0];
        let mut playback = SourcePlayback::for_asset("gun", 48_000, 0.0, signal.len(), true, true);
        playback.honor_loop_delay = true;
        playback.consume_retrigger_after(1, 2);
        let mut output = [0.0; 8];
        for sample in &mut output { *sample = playback.next_sample(&signal, true); }
        assert_eq!(output, [0.0, 0.0, 1.0, 2.0, 3.0, 1.0, 2.0, 3.0]);
        playback.consume_retrigger_after(2, 2);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
        playback.consume_scene_retrigger(3, 2, 7);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 0.0);
        assert_eq!(playback.next_sample(&signal, true), 1.0);
    }
}

impl WorkbenchApp {
    /// Runs the real Workbench graph without creating a window or an egui context.
    pub fn run_headless(args: LaunchArgs, startup_started: Instant) -> Result<(), String> {
        let options = args.replay.clone().ok_or("missing replay options")?;
        if !(-20..=40).contains(&options.monitor_gain_db) {
            return Err("replay monitor gain must be in -20..=40 dB".into());
        }
        if !(1..=120).contains(&options.seconds) || args.fixtures.len() != 1 || !args.start_audio {
            return Err(
                "headless replay requires 1..120 seconds, one fixture and explicit audio start"
                    .into(),
            );
        }
        if args.null_output && args.device.is_some() {
            return Err("--null-output is mutually exclusive with --device".into());
        }
        let requested_device = if args.null_output {
            "null-output".to_owned()
        } else {
            args.device
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("headless replay requires an explicit device")?
        };
        validate_replay_root(&options.capture_root)?;
        let paced_profile_out = std::env::var_os("FIGHTBOX_PACED_PROFILE_OUT").map(PathBuf::from);
        if let Some(path) = &paced_profile_out {
            validate_replay_root(path)?;
        }
        if let Some(path) = &args.render_out {
            validate_render_out(path)?;
            if !args.null_output {
                return Err("render output requires --null-output".into());
            }
        }
        if args.render_format == RenderFormat::Ambix
            && (args.render_out.is_none() || options.monitor_gain_db != 0)
        {
            return Err("AmbiX requires --render-out and calibrated monitor gain 0 dB".into());
        }
        let fixture = Fixture::read(&args.fixtures[0])?;
        let cued_scene = !fixture.cues.is_empty();
        if args.render_out.is_none() && fixture.sources.len() != 1 && !cued_scene {
            return Err("headless replay requires one source or a cued scene".into());
        }
        let authored = fixture.listener.trajectory.as_ref();
        let route = authored.map(SourceTrajectory::from_fixture).transpose()?;
        if let Some(route) = &route {
            if !route.speed_mps.is_finite() || route.speed_mps <= 0.0 {
                return Err("listener trajectory speed must be finite and positive".into());
            }
        }
        let mut app = Self::load(args.clone(), startup_started)?;
        let workbench = app.active.as_mut().ok_or("no active scene")?;
        if let Some(path) = &args.program_file {
            workbench.begin_song_load(path.clone(), None)?;
            let load = workbench.song_load.take().ok_or("missing song decoder")?;
            let result = load.receiver.recv().map_err(|error| format!("song decoder stopped: {error}"))?;
            workbench.finish_song_load(load.index, &load.path, result)?;
        }
        // A shot replay stands still and presses Play once before the device
        // starts, through the same route as the Spot and Listen buttons.
        let fixed_listener = match options.shot_spot {
            Some(spot) => {
                if !workbench.choose_listening_spot(spot.east_m()) {
                    return Err("--replay-shot requires the street comparison fixture".into());
                }
                if args.render_out.is_none() {
                    workbench.select_source_comparison(0, SourceComparisonMode::Spatial);
                }
                Some(workbench.listener.position)
            }
            None if cued_scene => {
                workbench.play_scene();
                route.is_none().then_some(workbench.listener.position)
            }
            None if route.is_none()
                || (args.render_out.is_none() && (args.live_input_wav.is_some() || args.program_file.is_some())) => {
                Some(workbench.listener.position)
            }
            None => None,
        };
        if !cued_scene && (args.render_out.is_some() || args.program_file.is_some()
            || workbench.sources.iter().any(|source| source.asset_id.starts_with("song:"))) {
            workbench.play_fixture_at_start();
        }
        let mut samples = Vec::new();
        let clock = Instant::now();
        samples.push(workbench.replay_tick(route.as_ref(), fixed_listener, 0, 0, clock));
        let bundle = workbench.capture.start(workbench.capture_draft())?;
        let paced_profile_before = paced_profile_out.as_ref().map(|_| {
            fightbox_steam_audio::reset_render_profile_histograms();
            let before = fightbox_steam_audio::render_profile_totals();
            fightbox_steam_audio::enable_render_profiling(true);
            before
        });
        workbench.audio = workbench
            .pending_audio
            .take()
            .ok_or("missing deferred graph")?();
        let actual_device: Option<String> = match &workbench.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(output) => Some(output.device_name().to_owned()),
            _ => None,
        };
        let mut errors = Vec::new();
        if actual_device.as_deref() != Some(requested_device.as_str()) {
            errors.push(match &workbench.audio {
                AudioState::Unavailable(error) => error.clone(),
                _ => format!("requested device {requested_device:?}, actual {actual_device:?}; refusing replay"),
            });
        }
        let target_blocks =
            u64::from(options.seconds) * u64::from(SAMPLE_RATE) / u64::from(BLOCK_SIZE);
        let mut last_block = 0;
        let mut last_progress = Instant::now();
        while errors.is_empty() && last_block < target_blocks {
            let block = workbench.audio_block_reader.read();
            if block != last_block {
                last_block = block;
                last_progress = Instant::now();
                samples.push(workbench.replay_tick(
                    route.as_ref(),
                    fixed_listener,
                    block,
                    samples.len() as u64,
                    clock,
                ));
            }
            if last_progress.elapsed().as_secs_f64() > 3.0
                || clock.elapsed().as_secs_f64() > f64::from(options.seconds) + 10.0
            {
                errors.push("audio callback stalled or replay wall-clock deadline exceeded".into());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Pause first, then close the tap and drain its writer. No callback can
        // append after the final block recorded below.
        if let Err(error) = workbench.stop_audio() {
            errors.push(error);
        }
        let final_audio_block = workbench.audio_block_reader.read();
        let stats = workbench.capture_end_stats();
        let late_blocks = stats.late_blocks;
        let original_safety = audio_safety_telemetry(&workbench.audio);
        let input_telemetry = match &workbench.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(output) => output.input_telemetry_json(),
            _ => serde_json::json!([]),
        };
        // Dropping the stream also closes it if the explicit pause failed.
        workbench.audio = AudioState::Stopped;
        if let Some(path) = &paced_profile_out {
            fightbox_steam_audio::enable_render_profiling(false);
            if let Err(error) = write_paced_render_profile(path, paced_profile_before.unwrap()) {
                errors.push(error);
            }
        }
        if stats.processing_errors != 0
            || stats.stream_errors != 0
            || stats.backend_render_error != 0
        {
            errors.push(
                "live output reported processing, stream, or backend faults (see manifest)".into(),
            );
        }
        if let Some(blocks) = original_safety
            .map(|safety| safety.non_finite_blocks)
            .filter(|blocks| *blocks != 0)
        {
            errors.push(format!(
                "output safety silenced {blocks} non-finite block(s)"
            ));
        }
        workbench.capture.request_stop();
        let drain_started = Instant::now();
        while !workbench.capture.ready_to_finish() && drain_started.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        if !workbench.capture.ready_to_finish() {
            errors.push("capture drain timed out".into());
        }
        workbench.capture.finish(stats)?;
        let finish_started = Instant::now();
        let mut completed = false;
        while finish_started.elapsed().as_secs() < 5 {
            if let Some(completion) = workbench.capture.poll_completion() {
                completed = true;
                if let Err(error) = completion.result {
                    errors.push(error);
                }
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        if !completed {
            errors.push("capture finalization timed out".into());
        }
        let dropped_blocks = workbench.capture.dropped_blocks();
        workbench.refresh_acoustic_feed();
        if dropped_blocks != 0 {
            errors.push(format!("capture dropped {dropped_blocks} blocks"));
        }
        let mut ambix_levels = None;
        if let Some(capture) = &workbench.ambix_capture {
            match capture.finish() {
                Ok(pcm) => {
                    let mut peak = 0.0_f32;
                    let mut squares = [0.0_f64; 9];
                    for frame in pcm.chunks_exact(9) {
                        for channel in 0..9 {
                            peak = peak.max(frame[channel].abs());
                            squares[channel] += f64::from(frame[channel]).powi(2);
                        }
                    }
                    let rms_dbfs = squares.map(|sum| amplitude_dbfs((sum / (pcm.len() / 9) as f64).sqrt() as f32));
                    ambix_levels = Some(serde_json::json!({
                        "peak_dbfs": amplitude_dbfs(peak), "channel_rms_dbfs": rms_dbfs,
                        "order": 2, "channels": 9, "ordering": "ACN", "normalization": "SN3D",
                        "sample_format": "float32", "axes": "+X listener front, +Y left, +Z up",
                        "safety": "calibrated source drive and source OutputSafety; reject peaks above -1 dBFS; no export limiter",
                        "bundle_audio": "ambix.wav; capture.wav is the silent stereo pacing transport",
                    }));
                    if errors.is_empty() && let Err(error) = crate::ambix::write_ambix_wav(&bundle.join("ambix.wav"), &pcm) {
                        errors.push(error);
                    }
                }
                Err(error) => errors.push(error),
            }
        }
        let scene_report = workbench.playback_status_reader.read().scene.map(|scene| serde_json::json!({
            "start_audio_sample": scene.start_audio_sample,
            "elapsed_frames": scene.frame,
            "cue_clock": "input audio samples; at_s rounded to nearest frame",
            "zone_clock": "once per run, first block observing listener inside",
            "cues": fixture.cues.iter().map(|cue| serde_json::json!({
                "at_s": cue.at_s, "play": cue.play, "stop": cue.stop,
                "when_listener_enters": cue.when_listener_enters.map(|zone| serde_json::json!({
                    "center_m": zone.center_m, "radius_m": zone.radius_m
                }))
            })).collect::<Vec<_>>()
        }));
        let report = serde_json::json!({
            "schema_version": 1,
            "acoustic_events": workbench.feed_history,
            "acoustic_feed_timing": "seconds from each trigger_audio_sample; audio sample timestamps from the consumed playback publication; ENU metres; three band gains are interaction pressure (0-0.8 / 0.8-8 / 8-22 kHz), not total listener energy",
            "requested_device": requested_device, "actual_device": actual_device,
            "fixture_path": args.fixtures[0], "waypoints_m": authored.map(|trajectory| &trajectory.waypoints_m),
            "speed_mps": authored.map(|trajectory| trajectory.speed_mps), "forward_enu": fixture.listener.forward_enu,
            "render_out": args.render_out, "late_blocks": late_blocks,
            "render_format": format!("{:?}", args.render_format).to_lowercase(),
            "ambix": ambix_levels,
            "late_blocks_semantics": "null-output blocks whose scheduled start was missed by at least one full block period; catch-up blocks run immediately",
            "playback_semantics": if args.render_out.is_some() && !cued_scene { "Play every fixture source once at audio t=0" } else if cued_scene { "Play scene at audio t=0" } else { "headless replay" },
            "sources": fixture.sources.iter().map(|source| serde_json::json!({
                "source_id": source.id, "source_position_m": source.position_m,
                "source_playback_start_offset_s": source.playback_start_offset_s,
                "source_trajectory": source.trajectory.as_ref().map(|trajectory| serde_json::json!({
                    "waypoints_m": trajectory.waypoints_m, "speed_mps": trajectory.speed_mps,
                    "max_speed_mps": trajectory.max_speed_mps
                }))
            })).collect::<Vec<_>>(),
            "source_id": fixture.sources[0].id, "source_position_m": fixture.sources[0].position_m,
            "source_playback_start_offset_s": fixture.sources[0].playback_start_offset_s,
            "source_trajectory": fixture.sources[0].trajectory.as_ref().map(|trajectory| serde_json::json!({
                "waypoints_m": trajectory.waypoints_m,
                "speed_mps": trajectory.speed_mps,
                "max_speed_mps": trajectory.max_speed_mps,
                "policy": "cyclic_closed_polyline",
                "height_policy": "live Workbench height selection; Street preserves initial authored altitude"
            })),
            "requested_seconds": options.seconds, "sample_rate_hz": SAMPLE_RATE, "block_size": BLOCK_SIZE,
            "monitor_gain_db": workbench.monitor_gain_db,
            "air": {
                "temperature_c": workbench.scene_air.observation().temperature_c,
                "relative_humidity_percent": workbench.scene_air.observation().relative_humidity_percent,
                "pressure_kpa": workbench.scene_air.observation().pressure_kpa,
                "pressure_exponents_per_m": workbench.scene_air_exponents,
            },
            "source_monitor_offset_db": workbench.sources[0].monitor_offset_db,
            "source_monitor_offsets_db": workbench.sources.iter().map(|source| source.monitor_offset_db).collect::<Vec<_>>(),
            "original_output_safety_cumulative": original_safety.map(|safety| serde_json::json!({
                "scope": if args.render_format == RenderFormat::Ambix { "ending spatial-route source safety telemetry; stem peak reported separately; no export limiter" } else { "ending cumulative original graph safety telemetry; not pre-limiter PCM; before optional quiet host guard" },
                "proximity_ceiling_engagements": safety.proximity_ceiling_engagements,
                "limiter_engagements": safety.limiter_engagements,
                "pre_limiter_peak": safety.pre_limiter_peak,
                "post_limiter_peak": safety.post_limiter_peak,
                "non_finite_blocks": safety.non_finite_blocks
            })),
            "live_input": input_telemetry,
            "live_input_wav": args.live_input_wav,
            "program_file": args.program_file,
            "listener_held_at_start": fixed_listener.is_some(),
            "scene": scene_report,
            "quiet_audition": workbench.quiet_provenance("replay_stopped"),
            "authored_reflection_output_enabled": fixture.simulation.reflections.enabled,
            "reflection_enable_semantics": "initial output-stage bypass only; simulation remains running",
            "control_timing_note": "2 ms polling; sampled_block drives listener and source routes. Publication block brackets are observed completed callback blocks, not exact adoption blocks. Acoustic telemetry is latest control-side state, not synchronized rays; energy sequence is independently observed.",
            "control_samples": samples, "final_audio_block": final_audio_block,
            "route_ended": fixed_listener.is_some() || route.as_ref().is_none_or(|route| route.sample_clamped_at_block(final_audio_block).1),
            "replay_shot": options.shot_spot.map(|spot| serde_json::json!({
                "spot": format!("{spot:?}"),
                "listener_position_m": [spot.east_m(), 483.82, 1.5],
                "semantics": "static listener; one Spatial Listen press before audio start"
            })),
            "status": if errors.is_empty() { "ended_and_stopped" } else { "failed_and_stopped" },
            "capture_dropped_blocks": dropped_blocks, "errors": errors,
        });
        std::fs::write(
            bundle.join("replay.json"),
            serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if errors.is_empty()
            && let Some(path) = &args.render_out
        {
            std::fs::copy(bundle.join(if args.render_format == RenderFormat::Ambix { "ambix.wav" } else { "capture.wav" }), path)
                .map_err(|e| format!("cannot export render WAV: {e}"))?;
            std::fs::write(
                path.with_extension("json"),
                serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
            )
            .map_err(|e| format!("cannot export render report: {e}"))?;
            println!("{}", path.display());
        }
        println!("{}", bundle.display());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

fn write_paced_render_profile(path: &std::path::Path, before: fightbox_steam_audio::RenderProfileTotals) -> Result<(), String> {
    let after = fightbox_steam_audio::render_profile_totals();
    let histogram = fightbox_steam_audio::render_profile_histograms();
    let stages = histogram.stage_names.iter().zip(&histogram.counts).map(|(name, counts)| {
        let count = counts.iter().sum::<u64>();
        let percentile = |fraction: f64| {
            let target = (count as f64 * fraction).ceil().max(1.0) as u64;
            let mut observed = 0_u64;
            counts.iter().enumerate().find_map(|(bucket, blocks)| {
                observed += blocks;
                (observed >= target).then_some(bucket)
            }).filter(|bucket| *bucket < counts.len() - 1)
                .map(|bucket| (bucket + 1) as f64 * histogram.bucket_width_ns as f64 / 1000.0)
        };
        serde_json::json!({ "stage": name, "blocks": count,
            "p50_us": percentile(0.5), "p99_us": percentile(0.99), "p999_us": percentile(0.999),
            "overflow_blocks": counts.last(), "histogram_counts": counts })
    }).collect::<Vec<_>>();
    let cpu_counts = &histogram.thread_cpu_counts;
    let cpu_count = cpu_counts.iter().sum::<u64>();
    let cpu_percentile = |fraction: f64| {
        let target = (cpu_count as f64 * fraction).ceil().max(1.0) as u64;
        let mut observed = 0_u64;
        cpu_counts.iter().enumerate().find_map(|(bucket, blocks)| {
            observed += blocks;
            (observed >= target).then_some(bucket)
        }).filter(|bucket| *bucket < cpu_counts.len() - 1)
            .map(|bucket| (bucket + 1) as f64 * histogram.bucket_width_ns as f64 / 1000.0)
    };
    let reflection_quality = ["Full", "Reduced", "Minimum", "Intermediate"].into_iter().enumerate()
        .map(|(index, level)| serde_json::json!({ "level": level,
            "histogram_counts": histogram.quality_counts[index],
            "backend_deadline_misses": histogram.quality_deadline_misses[index] }))
        .collect::<Vec<_>>();
    let report = serde_json::json!({ "schema_version": 1,
        "method": "paced render-stage histograms; concurrent simulation; startup warming excluded; no render-thread allocation or locks",
        "bucket_width_ns": histogram.bucket_width_ns,
        "overflow_lower_bound_ns": (histogram.counts[0].len() - 1) as u64 * histogram.bucket_width_ns,
        "percentile_semantics": "bucket upper bound; null means empty or overflow",
        "stages": stages, "first_block_stage_order": histogram.stage_names,
        "reflection_quality": reflection_quality,
        "reflection_detail": {
            "stage_order": ["total", "convolution", "mixer", "decode"],
            "histogram_counts_by_rung": histogram.reflection_counts_by_rung,
            "adoption_histogram_order": ["no_adoption_requested", "adoption_requested"],
            "adoption_histogram_counts": histogram.reflection_adoption_counts,
            "slow_block_columns": ["block", "rung", "applies", "adoption_requests", "held_applies", "reflection_ns", "convolution_ns", "mixer_ns", "decode_ns", "backend_cpu_ns"],
            "slow_blocks": histogram.slow_reflection_blocks
        },
        "first_blocks_ns": histogram.first_blocks_ns,
        "thread_cpu": { "available": histogram.thread_cpu_available,
            "blocks": cpu_count, "total_ns": histogram.thread_cpu_total_ns,
            "mean_us": (cpu_count != 0).then(|| histogram.thread_cpu_total_ns as f64 / cpu_count as f64 / 1000.0),
            "p50_us": cpu_percentile(0.5), "p99_us": cpu_percentile(0.99), "p999_us": cpu_percentile(0.999),
            "overflow_blocks": cpu_counts.last(), "histogram_counts": cpu_counts,
            "first_blocks_ns": histogram.first_blocks_thread_cpu_ns },
        "totals": { "total_ns": after.total_ns - before.total_ns,
            "preparation_ns": after.preparation_ns - before.preparation_ns,
            "direct_ns": after.direct_ns - before.direct_ns, "path_ns": after.path_ns - before.path_ns,
            "echo_ns": after.echo_ns - before.echo_ns, "reflection_ns": after.reflection_ns - before.reflection_ns,
            "block_count": after.block_count - before.block_count,
            "source_count": after.source_count - before.source_count,
            "echo_tap_count": after.echo_tap_count - before.echo_tap_count }
    });
    std::fs::create_dir_all(path.parent().ok_or("paced profile path has no parent")?).map_err(|e| e.to_string())?;
    std::fs::write(path, serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?)
        .map_err(|e| format!("cannot write paced render profile: {e}"))
}

fn validate_render_out(path: &std::path::Path) -> Result<(), String> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("wav") {
        return Err("--render-out must name an absolute .wav file".into());
    }
    validate_replay_root(path)?;
    validate_replay_root(&path.with_extension("json"))
}

fn validate_replay_root(root: &std::path::Path) -> Result<(), String> {
    if !root.is_absolute() {
        return Err("capture root must be absolute".into());
    }
    // Resolve existing ancestors first so symlinks cannot place captures inside
    // either this worktree or the canonical checkout.
    let mut ancestor = root;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        if ancestor.symlink_metadata().is_ok() {
            return Err("capture destination contains an unresolved symlink".into());
        }
        suffix.push(
            ancestor
                .file_name()
                .ok_or("invalid capture root")?
                .to_owned(),
        );
        ancestor = ancestor.parent().ok_or("invalid capture root")?;
    }
    let mut resolved = ancestor.canonicalize().map_err(|e| e.to_string())?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    if root
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("capture root must not contain parent-directory components".into());
    }
    let mut repos = vec![
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .map_err(|e| e.to_string())?,
    ];
    for flag in ["--show-toplevel", "--git-common-dir"] {
        if let Ok(output) = std::process::Command::new("git")
            .args(["rev-parse", flag])
            .output()
        {
            if output.status.success() {
                let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
                if let Ok(path) = path.canonicalize() {
                    repos.push(if flag == "--git-common-dir" {
                        path.parent().ok_or("invalid git directory")?.to_path_buf()
                    } else {
                        path
                    });
                }
            }
        }
    }
    if repos.iter().any(|repo| resolved.starts_with(repo)) {
        return Err("capture root must be outside repository trees".into());
    }
    Ok(())
}

impl Workbench {
    fn replay_tick(
        &mut self,
        route: Option<&SourceTrajectory>,
        fixed_listener: Option<EnuVector3>,
        block: u64,
        sequence: u64,
        clock: Instant,
    ) -> serde_json::Value {
        self.refresh_acoustic_feed();
        self.update_source_motion_at_block(block);
        let source_motion = self.source_motion[0];
        let (position, velocity, ended) = match fixed_listener {
            Some(position) => (position, EnuVector3::default(), true),
            None => {
                let route = route.expect("a moving replay listener has an authored route");
                let (sample, ended) = route.sample_clamped_at_block(block);
                let velocity = if ended {
                    EnuVector3::default()
                } else {
                    scale(sample.direction, route.speed_mps)
                };
                (sample.position, velocity, ended)
            }
        };
        self.listener.position = position;
        let before = self.audio_block_reader.read();
        self.publish_listener_control(velocity);
        let after = self.audio_block_reader.read();
        let acoustic = self.acoustic_telemetry.read();
        let energy = self.live_stage_energy.read();
        serde_json::json!({
            "sequence": sequence, "sampled_block": block,
            "publication_block_before": before, "publication_block_after": after,
            "wall_elapsed_s": clock.elapsed().as_secs_f64(),
            "position_m": [position.east_m, position.north_m, position.up_m],
            "source_position_m": [source_motion.pose.position.east_m, source_motion.pose.position.north_m, source_motion.pose.position.up_m],
            "source_velocity_mps": [source_motion.linear_velocity_mps.east_m, source_motion.linear_velocity_mps.north_m, source_motion.linear_velocity_mps.up_m],
            "source_forward_enu": [source_motion.pose.forward.east_m, source_motion.pose.forward.north_m, source_motion.pose.forward.up_m],
            "route_ended": ended, "acoustic_known": acoustic.known,
            "occlusion": acoustic.source_occlusion[0], "path_strength": acoustic.source_path_sh_energy[0],
            "path_eq": acoustic.source_path_eq[0],
            "source_quality": (acoustic.known && acoustic.governor_available)
                .then(|| format!("{:?}", acoustic.source_quality[0])),
            "governor": acoustic.governor.map(|g| serde_json::json!({
                "ladder_position": g.ladder_position, "reason": format!("{:?}", g.reason),
                "reflection_level": format!("{:?}", g.reflection_level),
                "reflection_rays": g.reflection_rays, "reflection_bounces": g.reflection_bounces,
                "reflection_ir_duration_s": g.reflection_ir_duration_s,
                "reflection_cadence_divisor": g.reflection_cadence_divisor,
                "reflection_output_gain": g.reflection_output_gain,
                "callback_deadline_misses": g.callback_deadline_misses,
                "render_p50_ns": g.render_p50_ns,
                "render_p99_ns": g.render_p99_ns,
                "render_p999_ns": g.render_p999_ns
            })),
            "live_stage_energy": { "sequence": energy.sequence, "simulation_sequence": energy.simulation_sequence,
                "world_generation": energy.world_generation, "audible_source_count": energy.audible_source_count,
                "direct_path_energy": energy.direct_path_energy, "reflection_energy": energy.reflection_energy }
        })
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    #[test]
    fn listener_replay_follows_corner_and_holds_end_without_wrapping() {
        let route = SourceTrajectory::from_fixture(&Trajectory {
            waypoints_m: vec![[0.0, 0.0, 1.5], [1.0, 0.0, 1.5], [1.0, 1.0, 1.5]],
            speed_mps: 1.0,
            max_speed_mps: None,
        })
        .unwrap();
        assert_eq!(
            route.sample_clamped_at_block(375).0.position,
            EnuVector3::new(1.0, 0.0, 1.5)
        );
        for block in [750, 1125, 7500] {
            let (sample, ended) = route.sample_clamped_at_block(block);
            assert!(ended);
            assert_eq!(sample.position, EnuVector3::new(1.0, 1.0, 1.5));
            assert_eq!(sample.direction, EnuVector3::default());
        }
    }

    #[test]
    fn replay_and_render_paths_reject_repositories_and_symlink_destinations() {
        assert!(validate_replay_root(std::path::Path::new("captures")).is_err());
        assert!(validate_render_out(std::path::Path::new("render.wav")).is_err());
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        assert!(validate_replay_root(&repo.join("capture-do-not-create")).is_err());
        assert!(validate_render_out(&repo.join("render-do-not-create.wav")).is_err());
        assert!(!repo.join("capture-do-not-create").exists());
        let temp =
            std::env::temp_dir().join(format!("fightbox-render-path-{}", std::process::id()));
        std::fs::create_dir(&temp).unwrap();
        assert!(validate_render_out(&temp.join("new-parent/output.wav")).is_ok());
        assert!(validate_render_out(&temp.join("../output.wav")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&repo, temp.join("repo-link")).unwrap();
            assert!(validate_render_out(&temp.join("repo-link/output.wav")).is_err());
            std::os::unix::fs::symlink(repo.join("Cargo.toml"), temp.join("output.json")).unwrap();
            assert!(validate_render_out(&temp.join("output.wav")).is_err());
            std::os::unix::fs::symlink(repo.join("not-created.wav"), temp.join("dangling.wav"))
                .unwrap();
            assert!(validate_render_out(&temp.join("dangling.wav")).is_err());
        }
        std::fs::remove_dir_all(temp).unwrap();
    }
}

#[cfg(test)]
mod live2d_scene_evidence {
    use super::*;
    #[test]
    #[ignore = "retained package/media; set FIGHTBOX_LIVE2D_ARTIFACT_DIR outside repository"]
    fn source_placement_offscreen_never_enables_and_saves() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let evidence = PathBuf::from("/path/to/spatial-audio/evidence");
        let output = PathBuf::from(std::env::var("FIGHTBOX_LIVE2D_ARTIFACT_DIR").unwrap());
        validate_replay_root(&output).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let fixture_path = output.join("placement-scene.json");
        std::fs::copy(
            root.join("fixtures/city/combat-reference/fixture.json"),
            &fixture_path,
        )
        .unwrap();
        let args = LaunchArgs {
            package: evidence.join("megablock-seed1/megablock.fightbox"),
            baked: evidence
                .join("astra-user-weak-street/road-sample-matrix/successor-path1500-vis40.baked"),
            fixtures: vec![fixture_path.clone()],
            start_audio: false,
            null_output: false,
            render_out: None,
            render_format: RenderFormat::Binaural,
            live_input_wav: None,
            program_file: None,
            device: None,
            replay: None,
            quiet_audition: None,
        };
        let mut app = WorkbenchApp::load(args, Instant::now()).unwrap();
        let workbench = app.active.as_mut().unwrap();
        workbench.listening_mode = false;
        workbench.ground_map_enabled = true;
        workbench.ground_map_whole_scene = true;
        workbench.anomaly_field.selected_source = 1;
        let source = workbench.sources[1].position;
        assert!(workbench.sources.iter().all(|source| !source.enabled));
        let ctx = egui::Context::default();
        let mut raster = crate::ground_map::offscreen::Raster::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 820.0));
        // Render the actual painter with a stable viewport so pointer replay uses
        // exactly its metric projection, including panel and map insets.
        let map_rect = Rect::from_min_max(Pos2::new(28.0, 80.0), Pos2::new(900.0, 736.0));
        let bounds = workbench.ground_map.bounds;
        let projection = crate::ground_map::MapProjection::new(bounds, map_rect);
        let origin = projection.project([source.east_m, source.north_m]);
        let run = |workbench: &mut Workbench,
                   raster: &mut crate::ground_map::offscreen::Raster,
                   events: Vec<egui::Event>,
                   name: &str| {
            let mut save_rect = Rect::NOTHING;
            let frame = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(928.0, 800.0));
                        let painter = ui.painter_at(rect);
                        workbench.draw_ground_map(ui, &painter, rect);
                        ui.allocate_rect(
                            Rect::from_min_size(Pos2::new(20.0, 20.0), egui::vec2(260.0, 32.0)),
                            Sense::hover(),
                        );
                        save_rect = workbench.scene_save_controls(ui);
                    });
                },
            );
            let triangles = raster.save(&ctx, frame, &output.join(format!("{name}.png")));
            assert!(triangles > 100);
            assert!(matches!(workbench.audio, AudioState::Stopped));
            assert!(workbench.sources.iter().all(|source| !source.enabled));
            save_rect
        };
        run(workbench, &mut raster, vec![], "placement-before");
        run(
            workbench,
            &mut raster,
            vec![
                egui::Event::PointerMoved(origin),
                egui::Event::PointerButton {
                    pos: origin,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            "placement-press",
        );
        let covered = projection.project([source.east_m + 8.0, source.north_m]);
        run(
            workbench,
            &mut raster,
            vec![egui::Event::PointerMoved(covered)],
            "placement-green",
        );
        assert!(workbench.source_drag.as_ref().unwrap().covered);
        let placed = workbench.sources[1].position;
        assert!((placed.east_m - source.east_m - 8.0).abs() < 0.001);
        let uncovered = projection.project([5.0, 5.0]);
        run(
            workbench,
            &mut raster,
            vec![egui::Event::PointerMoved(uncovered)],
            "placement-red",
        );
        assert!(!workbench.source_drag.as_ref().unwrap().covered);
        let save_rect = run(
            workbench,
            &mut raster,
            vec![egui::Event::PointerButton {
                pos: uncovered,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            "placement-drop",
        );
        assert!(workbench.source_drag.is_none());
        assert_eq!(workbench.sources[1].position, placed);
        assert_eq!(workbench.source_motion[1].pose.position, placed);
        assert!(workbench.scene_positions.is_dirty());
        let save = save_rect.center();
        run(
            workbench,
            &mut raster,
            vec![
                egui::Event::PointerMoved(save),
                egui::Event::PointerButton {
                    pos: save,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            "placement-save-press",
        );
        run(
            workbench,
            &mut raster,
            vec![egui::Event::PointerButton {
                pos: save,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            "placement-saved",
        );
        assert_eq!(workbench.scene_save_status.as_deref(), Some("Saved"));
        let saved = Fixture::read(&fixture_path).unwrap();
        assert_eq!(saved.sources[1].initial_position().unwrap(), placed);
        assert!(saved.sources.iter().all(|source| !source.default_enabled));
        assert!(workbench.sources.iter().all(|source| !source.enabled));
        assert!(!workbench.scene_positions.is_dirty());
        std::fs::write(
            output.join("placement.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "source": workbench.sources[1].id,
                "before_m": [source.east_m, source.north_m, source.up_m],
                "after_m": [placed.east_m, placed.north_m, placed.up_m],
                "sources_enabled": false, "audio_device_opened": false,
                "uncovered_drop": "last covered position", "saved_fixture": fixture_path,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// Opt-in retained-media/SDK test. Runs the actual combined scene and UI,
    /// never eframe::run_native or an audio device. Artifacts must live outside repos.
    #[test]
    #[ignore = "retained urban package/media; set FIGHTBOX_LIVE2D_ARTIFACT_DIR outside repository"]
    fn live2d_combined_scene_offscreen() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let output = PathBuf::from(
            std::env::var("FIGHTBOX_LIVE2D_ARTIFACT_DIR")
                .expect("explicit external evidence directory"),
        );
        validate_replay_root(&output).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let retained = root.parent().unwrap().join("evidence/megablock-seed1");
        let street_candidate = std::env::var("FIGHTBOX_STREET_CANDIDATE").as_deref() == Ok("1");
        let quiet_ready = !street_candidate && std::env::var_os("FIGHTBOX_QUIET_READY").is_some();
        let street_fixture = root.join("fixtures/city/astra-artillery/street-path-candidate.json");
        let args = LaunchArgs {
            package: retained.join("megablock.fightbox"),
            baked: if street_candidate {
                root.parent().unwrap().join("evidence/astra-user-weak-street/road-sample-matrix/successor-path1500-vis40.baked")
            } else {
                std::env::var_os("FIGHTBOX_LIVE2D_BAKED")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| retained.join("megablock.baked"))
            },
            fixtures: vec![if street_candidate {
                street_fixture.clone()
            } else {
                std::env::var_os("FIGHTBOX_LIVE2D_FIXTURE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| root.join("fixtures/city/astra-urban/fixture.json"))
            }],
            start_audio: quiet_ready,
            null_output: false,
            render_out: None,
            render_format: RenderFormat::Binaural,
            live_input_wav: None,
            program_file: None,
            device: quiet_ready.then(|| "Quiet ready sentinel must never open".to_owned()),
            replay: None,
            quiet_audition: quiet_ready.then_some(crate::QuietAuditionOptions { seconds: 10 }),
        };
        let mut app = WorkbenchApp::load(args, Instant::now()).unwrap();
        let workbench = app.active.as_mut().unwrap();
        assert_eq!(
            workbench.sources.len(),
            if street_candidate { 1 } else { 3 },
            "must exercise the selected retained production scene"
        );
        if street_candidate {
            let fixture = Fixture::read(&street_fixture).unwrap();
            let authored = &fixture.sources[0];
            assert_eq!(authored.asset_id, "astra-artillery-single");
            assert_eq!(authored.reference_level.db_spl, 155.0);
            assert_eq!(
                authored.extent,
                fightbox_api::ExtentDescriptor::LineSegment { length_m: 6.0 }
            );
            assert!(authored.restart_on_enable && !authored.default_enabled);
            assert_eq!(workbench.sources[0].declared_spl_at_one_meter_db, 155.0);
            assert_eq!(
                workbench.sources[0].position,
                EnuVector3::new(102.5, 102.5, 1.5)
            );
            assert_eq!(
                workbench.listener.position,
                EnuVector3::new(426.02, 483.82, 1.5)
            );
            assert!((workbench.listener.yaw_radians.to_degrees() - 84.0).abs() < 0.001);
            assert_eq!(workbench.monitor_gain_db, 30.0);
            assert!(!workbench.autopilot.enabled);
            assert_eq!(workbench.visibility_range.effective_m, 20.0);
            assert!(workbench.quiet_output.is_none() && workbench.pending_audio.is_none());
            assert!(
                workbench.host_echoes.is_some(),
                "the impulsive one-shot street source plans its own echoes"
            );
        }
        assert!(!workbench.ground_map_enabled);
        assert_eq!(workbench.listening_mode, street_candidate);
        assert!(matches!(workbench.audio, AudioState::Stopped));
        assert!(workbench.sources.iter().all(|source| !source.enabled));
        // Exercise the exact demo Listen/Stop actions against conflicting state;
        // only control publications change, never the deferred/device route.
        for source in &mut workbench.sources {
            source.enabled = true;
            source.muted = true;
            source.soloed = true;
        }
        let comparison_index = if street_candidate { 0 } else { 1 };
        let previous_generation = workbench.sources[comparison_index].retrigger_generation;
        workbench.select_source_comparison(comparison_index, SourceComparisonMode::Spatial);
        assert_eq!(
            workbench.sources[comparison_index].retrigger_generation,
            previous_generation.wrapping_add(1)
        );
        let gains = SourceMix::from_sources(&workbench.sources).gains(workbench.sources.len());
        assert!(gains[comparison_index] > 0.0);
        assert!(
            gains
                .iter()
                .enumerate()
                .all(|(index, gain)| index == comparison_index || *gain == 0.0)
        );
        assert!(!workbench.sources[comparison_index].muted);
        assert_eq!(
            workbench.source_comparison,
            Some(SourceComparison {
                source_index: comparison_index,
                mode: SourceComparisonMode::Spatial
            })
        );
        workbench.stop_demo_sources();
        assert!(
            workbench
                .sources
                .iter()
                .all(|source| !source.enabled && !source.soloed)
        );
        assert!(workbench.source_comparison.is_none());
        for source in &mut workbench.sources {
            source.muted = false;
        }
        assert!(matches!(workbench.audio, AudioState::Stopped));
        if quiet_ready {
            assert!(workbench.quiet_ready());
            assert_eq!(
                workbench
                    .quiet_output
                    .as_ref()
                    .unwrap()
                    .read()
                    .processed_frames,
                0
            );
        }
        if street_candidate {
            let yaw = workbench.listener.yaw_radians;
            let gain = workbench.monitor_gain_db;
            let offsets: Vec<_> = workbench
                .sources
                .iter()
                .map(|s| s.monitor_offset_db)
                .collect();
            workbench.select_source_comparison(0, SourceComparisonMode::Spatial);
            workbench.autopilot.enabled = true;
            assert!(workbench.choose_listening_spot(434.02));
            assert_eq!(
                workbench.listener.position,
                EnuVector3::new(434.02, 483.82, 1.5)
            );
            assert!(!workbench.autopilot.enabled);
            assert!(workbench.sources.iter().all(|s| !s.enabled && !s.soloed));
            assert!(workbench.choose_listening_spot(438.02));
            assert_eq!(workbench.listener.yaw_radians, yaw);
            assert_eq!(workbench.monitor_gain_db, gain);
            assert_eq!(
                workbench
                    .sources
                    .iter()
                    .map(|s| s.monitor_offset_db)
                    .collect::<Vec<_>>(),
                offsets
            );
            let held = workbench.listener.position;
            workbench.capture_state = CaptureUiState::Recording {
                bundle: output.join("guard-only-no-capture"),
            };
            assert!(!workbench.choose_listening_spot(434.02));
            assert_eq!(workbench.listener.position, held);
            workbench.capture_state = CaptureUiState::Idle;
            assert!(!workbench.choose_listening_spot(0.0));
        }
        let ctx = egui::Context::default();
        let mut raster = crate::ground_map::offscreen::Raster::default();
        let mut proof = Vec::new();
        let views: &[(&str, bool, usize, bool)] = if street_candidate {
            &[
                ("street-listening-a", true, 0, false),
                ("street-listening-b", true, 0, false),
                ("street-walk", false, 0, false),
                ("street-diagnostics", true, 0, false),
                ("street-live2d-whole", true, 0, true),
            ]
        } else {
            &[
                ("default-firstperson", false, 0, false),
                ("live2d-ground", true, 0, false),
                ("live2d-ground-whole", true, 0, true),
                ("live2d-second-source", true, 1, false),
                ("live2d-elevated", true, 2, false),
                ("measured-trace-synthetic", true, 0, false),
            ]
        };
        for &(name, map, selected, whole_scene) in views {
            if street_candidate {
                workbench.listening_mode =
                    name.starts_with("street-listening") || name == "street-walk";
                if name == "street-listening-a" {
                    assert!(workbench.choose_listening_spot(434.02));
                }
                if name == "street-listening-b" {
                    assert!(workbench.choose_listening_spot(438.02));
                }
            }
            if name == "measured-trace-synthetic" {
                let (mut writer, reader) =
                    crate::level_trace::channel(SAMPLE_RATE, 64, 64).unwrap();
                let origin = workbench.listener.position;
                for index in 0..20 {
                    let level = if index < 10 { 0.1 } else { 0.01 };
                    let pcm = vec![level; (SAMPLE_RATE / 10) as usize];
                    writer.observe(
                        &pcm,
                        &pcm,
                        crate::level_trace::BlockContext {
                            recording: true,
                            identity: crate::level_trace::TraceIdentity {
                                generation: 1,
                                epoch: 1,
                            },
                            listener_position_m: [
                                origin.east_m + index as f32,
                                origin.north_m + 8.0,
                                origin.up_m,
                            ],
                        },
                    );
                }
                writer.flush();
                workbench.level_trace = crate::level_trace_ui::LevelTraceUi::new(reader);
                workbench.level_trace.update();
                assert_eq!(workbench.level_trace.reader.samples().len(), 20);
            }
            workbench.ground_map_enabled = map;
            workbench.ground_map_whole_scene = whole_scene;
            workbench.anomaly_field.selected_source = selected;
            // Exercise bright active contours without opening an audio device.
            // This is UI state only; the default startup above remains all-off.
            for source in &mut workbench.sources {
                source.enabled = false;
            }
            if map && !workbench.listening_mode {
                workbench.sources[selected].enabled = true;
            }
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 820.0))),
                ..Default::default()
            };
            let frame = ctx.run(input, |ctx| workbench.update_ui(ctx));
            let triangles = raster.save(&ctx, frame, &output.join(format!("{name}.png")));
            assert!(triangles > 100, "actual UI must render geometry and text");
            assert!(matches!(workbench.audio, AudioState::Stopped));
            if quiet_ready {
                assert!(
                    workbench.quiet_ready(),
                    "rendering must not consume deferred startup"
                );
                assert_eq!(
                    workbench
                        .quiet_output
                        .as_ref()
                        .unwrap()
                        .read()
                        .processed_frames,
                    0
                );
            }
            proof.push(serde_json::json!({"view":name,"street_candidate":street_candidate,"listening_mode":workbench.listening_mode,"listener_east_m":workbench.listener.position.east_m,"trace_pcm_is_synthetic_not_city_output":name == "measured-trace-synthetic","selected_source":workbench.sources[selected].id,"source_height_m":workbench.sources[selected].position.up_m,"source_enabled_for_visual_only":workbench.sources[selected].enabled,"triangles":triangles,"audio_device_opened":false,"quiet_ready":quiet_ready,"framing":if whole_scene {"whole_scene"} else {"local_sound"}}));
        }
        std::fs::write(
            output.join("offscreen.json"),
            serde_json::to_vec_pretty(&proof).unwrap(),
        )
        .unwrap();
    }

    /// Opt-in look at any launcher's scene as it first opens, offscreen, with no
    /// device or window.
    #[test]
    #[ignore = "set FIGHTBOX_SHOT_PACKAGE, FIGHTBOX_SHOT_BAKED, FIGHTBOX_SHOT_FIXTURE and FIGHTBOX_LIVE2D_ARTIFACT_DIR"]
    fn launcher_offscreen() {
        let var = |name| PathBuf::from(std::env::var(name).unwrap());
        let output = var("FIGHTBOX_LIVE2D_ARTIFACT_DIR");
        validate_replay_root(&output).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let args = LaunchArgs {
            package: var("FIGHTBOX_SHOT_PACKAGE"),
            baked: var("FIGHTBOX_SHOT_BAKED"),
            fixtures: vec![var("FIGHTBOX_SHOT_FIXTURE")],
            start_audio: false,
            null_output: false,
            render_out: None,
            render_format: RenderFormat::Binaural,
            live_input_wav: None,
            program_file: None,
            device: None,
            replay: None,
            quiet_audition: None,
        };
        let mut app = WorkbenchApp::load(args, Instant::now()).unwrap();
        let workbench = app.active.as_mut().unwrap();
        let ctx = egui::Context::default();
        let mut raster = crate::ground_map::offscreen::Raster::default();
        for (name, map) in [("opened", false), ("opened-map", true)] {
            workbench.ground_map_enabled = map;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 820.0))),
                ..Default::default()
            };
            let frame = ctx.run(input, |ctx| workbench.update_ui(ctx));
            raster.save(&ctx, frame, &output.join(format!("{name}.png")));
            assert!(matches!(workbench.audio, AudioState::Stopped));
        }
    }

    /// Opt-in walk-view design renders (A map, B walk, C earshot) at phone and
    /// desktop size, offscreen, with no device or window. Output goes to
    /// `FIGHTBOX_LIVE2D_ARTIFACT_DIR/<A|B|C>/`.
    #[test]
    #[ignore = "set FIGHTBOX_SHOT_PACKAGE, FIGHTBOX_SHOT_BAKED, FIGHTBOX_SHOT_FIXTURE and FIGHTBOX_LIVE2D_ARTIFACT_DIR"]
    fn walk_view_offscreen() {
        use crate::walk_view::WalkDesign;
        let var = |name| PathBuf::from(std::env::var(name).unwrap());
        let output = var("FIGHTBOX_LIVE2D_ARTIFACT_DIR");
        validate_replay_root(&output).unwrap();
        let args = LaunchArgs {
            package: var("FIGHTBOX_SHOT_PACKAGE"),
            baked: var("FIGHTBOX_SHOT_BAKED"),
            fixtures: vec![var("FIGHTBOX_SHOT_FIXTURE")],
            start_audio: false,
            null_output: false,
            render_out: None,
            render_format: RenderFormat::Binaural,
            live_input_wav: None,
            program_file: None,
            device: None,
            replay: None,
            quiet_audition: None,
        };
        let mut app = WorkbenchApp::load(args, Instant::now()).unwrap();
        let workbench = app.active.as_mut().unwrap();
        assert!(
            !workbench.walk.atlas.names.is_empty(),
            "fixture must carry street_names"
        );
        workbench.listening_mode = true;
        workbench.walk_preview = true;
        let spawn = workbench.listener;
        // Visual only: Music looks playing. The audio stream stays closed.
        let music = workbench
            .sources
            .iter()
            .position(|source| source.id == "music")
            .unwrap_or(0);
        workbench.sources[music].enabled = true;
        workbench.anomaly_field.selected_source = music;
        let forward = spawn.forward();
        let right = spawn.right();
        let place = [
            spawn.position.east_m + forward.east_m * 34.0 + right.east_m * 14.0,
            spawn.position.north_m + forward.north_m * 34.0 + right.north_m * 14.0,
        ];
        // A street corner near the spawn, facing up the named street.
        let corner = workbench
            .walk
            .atlas
            .corners
            .iter()
            .map(|(point, _)| *point)
            .min_by(|a, b| {
                crate::walk_view::distance(*a, [spawn.position.east_m, spawn.position.north_m])
                    .total_cmp(&crate::walk_view::distance(
                        *b,
                        [spawn.position.east_m, spawn.position.north_m],
                    ))
            })
            .unwrap();
        let ctx = egui::Context::default();
        let mut raster = crate::ground_map::offscreen::Raster::default();
        let mut proof = Vec::new();
        for design in WalkDesign::ALL {
            let directory = output.join(design.letter());
            std::fs::create_dir_all(&directory).unwrap();
            workbench.walk.design = Some(design);
            for (name, size, pixels_per_point, at_corner, placing) in [
                ("phone", egui::vec2(390.0, 844.0), 2.0, false, None),
                ("phone-corner", egui::vec2(390.0, 844.0), 2.0, true, None),
                (
                    "phone-place",
                    egui::vec2(390.0, 844.0),
                    2.0,
                    false,
                    Some(place),
                ),
                ("desktop", egui::vec2(1280.0, 820.0), 1.0, false, None),
            ] {
                workbench.listener = spawn;
                if at_corner {
                    workbench.listener.position.east_m = corner[0] - 6.0;
                    workbench.listener.position.north_m = corner[1] + 22.0;
                    workbench.listener.yaw_radians = std::f32::consts::PI;
                }
                workbench.walk.placing = placing;
                let mut input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                    ..Default::default()
                };
                input.viewports.insert(
                    egui::ViewportId::ROOT,
                    egui::ViewportInfo {
                        native_pixels_per_point: Some(pixels_per_point),
                        ..Default::default()
                    },
                );
                let path = directory.join(format!("{name}.png"));
                // The first pass settles layout; the second is the picture.
                let mut triangles = 0;
                for _ in 0..2 {
                    let frame = ctx.run(input.clone(), |ctx| workbench.update_ui(ctx));
                    triangles = raster.save(&ctx, frame, &path);
                }
                assert!(triangles > 100, "walk view must render geometry and text");
                assert!(matches!(workbench.audio, AudioState::Stopped));
                proof.push(serde_json::json!({
                    "design": design.letter(),
                    "view": name,
                    "points": [size.x, size.y],
                    "pixels_per_point": pixels_per_point,
                    "listener_m": [workbench.listener.position.east_m, workbench.listener.position.north_m],
                    "place_note": workbench.walk.atlas.place_note([workbench.listener.position.east_m, workbench.listener.position.north_m]).here,
                    "triangles": triangles,
                    "audio_device_opened": false,
                }));
            }
        }
        workbench.listener = spawn;
        // D (the Apple Maps mock) draws these same pins, at the spawn.
        let walk_d = output.join("D");
        std::fs::create_dir_all(&walk_d).unwrap();
        let air_db_per_m = workbench.scene_air_exponents[1] * 8.685_89;
        let listener = [spawn.position.east_m, spawn.position.north_m];
        let note = workbench.walk.atlas.place_note(listener);
        let pins = workbench
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let size = workbench.walk_size(index, air_db_per_m);
                let distance_m = vector_length(subtract(source.position, spawn.position));
                let ground = [source.position.east_m, source.position.north_m];
                let rise = source.position.up_m - spawn.position.up_m;
                let color = crate::walk_view::pin_color(index);
                serde_json::json!({
                    "id": source.id,
                    "label": source.audition_label,
                    "position_m": [source.position.east_m, source.position.north_m, source.position.up_m],
                    "color_rgb": [color.r(), color.g(), color.b()],
                    "on": source.enabled,
                    "selected": index == music,
                    "moving": source.trajectory.is_some(),
                    "size": size.label(),
                    "spl_at_one_m_db": size.spl_at_one_m_db,
                    "width_m": size.width_m,
                    "reach_m": size.reach_m,
                    "distance_m": distance_m,
                    "direction": if rise > 2.0 * crate::walk_view::distance(ground, listener) {
                        "overhead"
                    } else {
                        crate::walk_view::relative_direction(listener, spawn.yaw_radians, ground)
                    },
                    "level_here_db": crate::walk_view::open_air_level_db(
                        size.spl_at_one_m_db,
                        distance_m.max(size.width_m * 0.5),
                        air_db_per_m,
                    ),
                })
            })
            .collect::<Vec<_>>();
        std::fs::write(
            walk_d.join("pins.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "listener_m": [spawn.position.east_m, spawn.position.north_m, spawn.position.up_m],
                "yaw_radians": spawn.yaw_radians,
                "place_note": { "here": note.here, "near": note.near },
                "air_db_per_m": air_db_per_m,
                "street_ambient_db": crate::walk_view::STREET_AMBIENT_DB_SPL,
                "pins": pins,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            output.join("walk-view.json"),
            serde_json::to_vec_pretty(&proof).unwrap(),
        )
        .unwrap();
    }

    /// The City Map link against the real scene: a loopback client reads the
    /// hello and state, then drives moves, volume and toggles through the
    /// same Workbench actions. Writes `link/map-hello.json` and
    /// `link/map-state.json` for the map's offscreen renders.
    #[test]
    #[ignore = "set FIGHTBOX_SHOT_PACKAGE, FIGHTBOX_SHOT_BAKED, FIGHTBOX_SHOT_FIXTURE and FIGHTBOX_LIVE2D_ARTIFACT_DIR"]
    fn city_map_link_offscreen() {
        use std::io::{BufRead, BufReader, Write};
        let var = |name| PathBuf::from(std::env::var(name).unwrap());
        let output = var("FIGHTBOX_LIVE2D_ARTIFACT_DIR");
        validate_replay_root(&output).unwrap();
        let package = var("FIGHTBOX_SHOT_PACKAGE");
        let args = LaunchArgs {
            package: package.clone(),
            baked: var("FIGHTBOX_SHOT_BAKED"),
            fixtures: vec![var("FIGHTBOX_SHOT_FIXTURE")],
            start_audio: false,
            null_output: false,
            render_out: None,
            render_format: RenderFormat::Binaural,
            live_input_wav: None,
            program_file: None,
            device: None,
            replay: None,
            quiet_audition: None,
        };
        let mut app = WorkbenchApp::load(args, Instant::now()).unwrap();
        let workbench = app.active.as_mut().unwrap();
        let music = workbench
            .sources
            .iter()
            .position(|source| source.id == "music")
            .unwrap();
        // The party PA plays a song, as when one is dropped on the speaker
        // (the scenes' own music is live system audio, which this test never
        // opens). Decoded on the control side; no audio device.
        if let Ok(song) = std::env::var("FIGHTBOX_MAP_MUSIC_SONG") {
            let song = PathBuf::from(song);
            let asset = crate::asset::prepare_song(&song, false);
            workbench.finish_song_load(music, &song, asset).unwrap();
        }
        // Visual only: Music looks playing. The audio stream stays closed.
        workbench.sources[music].enabled = true;
        workbench.anomaly_field.selected_source = music;

        let mut link = crate::map_link::MapLink::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let client = std::net::TcpStream::connect(link.address).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let serve = |workbench: &mut Workbench, link: &mut crate::map_link::MapLink| {
            for _ in 0..3 {
                workbench.map.last_sent = None;
                workbench.serve_map_link(link, &package);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let started = Instant::now();
        serve(workbench, &mut link);
        let hello_ms = started.elapsed().as_millis();
        // The latest line of every kind read so far (music lines arrive
        // between the ones the test waits for).
        let stash = std::cell::RefCell::new(std::collections::BTreeMap::<String, String>::new());
        let mut read = |kind: &str| loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .unwrap_or_else(|error| panic!("waiting for a {kind} line: {error}"));
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if let Some(found) = value["type"].as_str() {
                stash.borrow_mut().insert(found.to_owned(), line.clone());
            }
            if value["type"] == kind {
                break (line, value);
            }
        };
        let (hello_line, hello) = read("hello");
        let (state_line, state) = read("state");
        let link_dir = output.join("link");
        std::fs::create_dir_all(&link_dir).unwrap();
        std::fs::write(link_dir.join("map-hello.json"), &hello_line).unwrap();
        std::fs::write(link_dir.join("map-state.json"), &state_line).unwrap();

        assert_eq!(hello["protocol"], crate::map_link::PROTOCOL);
        assert!(hello["origin"]["latitude_deg"].is_number(), "scene geo origin");
        let dots = hello["dots"]["points"].as_array().unwrap();
        let full = dots.iter().filter(|dot| dot[2] == 1.0).count();
        assert_eq!(hello["dots"]["source"], "baked probes");
        assert!(full > 1000 && dots.len() > full, "{full} full of {}", dots.len());
        let spots = hello["spots"].as_array().unwrap();
        assert!(spots.len() >= 3, "{spots:?}");
        assert_eq!(state["selected"], "music");
        assert_eq!(state["sources"][music]["on"], true);
        let heights = workbench
            .probe_points
            .iter()
            .fold(std::collections::BTreeMap::new(), |mut bins, point| {
                *bins.entry((point[2] / 2.0).floor() as i32 * 2).or_insert(0) += 1;
                bins
            });

        let send = |line: &str| {
            (&client).write_all(line.as_bytes()).unwrap();
            (&client).write_all(b"\n").unwrap();
        };
        // A spot pick by key, as the map's spot buttons send it: the street
        // corner, 1.5 m up.
        let corner = spots
            .iter()
            .find(|spot| spot["key"] == "street-corner")
            .unwrap_or(&spots[0]);
        send(&serde_json::json!({"type": "spot", "id": "music", "key": corner["key"]}).to_string());
        send(r#"{"type":"set_volume","db":12}"#);
        serve(workbench, &mut link);
        let moved = workbench.sources[music].position;
        assert!(
            (moved.east_m - corner["east_m"].as_f64().unwrap() as f32).abs() < 0.01
                && (moved.north_m - corner["north_m"].as_f64().unwrap() as f32).abs() < 0.01,
            "music moved to the corner: {moved:?}"
        );
        assert_eq!(workbench.monitor_gain_db, 12.0);
        assert!(workbench.scene_positions.is_dirty());

        // Refusals come back as notices and change nothing.
        send(r#"{"type":"move","id":"music","east_m":5000,"north_m":5000,"done":true}"#);
        send(r#"{"type":"move","id":"helicopter","east_m":0,"north_m":0,"done":true}"#);
        send(r#"{"type":"set_volume","db":99}"#);
        send(r#"{"type":"set_on","id":"bells","on":true}"#);
        serve(workbench, &mut link);
        assert_eq!(workbench.sources[music].position, moved);
        assert_eq!(workbench.monitor_gain_db, MAX_MONITOR_GAIN_DB);
        assert!(!workbench.sources[1].enabled, "no audio stream, so no play");
        let mut notices = Vec::new();
        while notices.len() < 3 {
            let (_, notice) = read("notice");
            notices.push(notice["text"].as_str().unwrap().to_owned());
        }
        std::fs::write(
            link_dir.join("link-proof.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "hello_bytes": hello_line.len(),
                "hello_first_turn_ms": hello_ms,
                "probe_count": workbench.probe_points.len(),
                "probe_height_bins_m": heights,
                "dots": dots.len(),
                "full_dots": full,
                "culled_marks": hello["dots"]["culled"],
                "buildings": hello["buildings"].as_array().map_or(0, Vec::len),
                "streets": hello["streets"].as_array().map_or(0, Vec::len),
                "spots": spots,
                "moved_music_to": [moved.east_m, moved.north_m, moved.up_m],
                "notices": notices,
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(notices.iter().any(|text| text.starts_with("No baked path")), "{notices:?}");
        assert!(notices.iter().any(|text| text.contains("flight path")), "{notices:?}");
        assert!(notices.iter().any(|text| text.contains("ready to play")), "{notices:?}");

        // Scenes with Willis Tower: "on the top of Sears Tower, really,
        // really loud". The trim rises within its slider range and goes back
        // the moment the sound is dragged anywhere else.
        if let Some(sears) = spots.iter().find(|spot| spot["key"] == "sears-tower") {
            let before = workbench.sources[music].monitor_offset_db;
            send(r#"{"type":"spot","id":"music","key":"sears-tower"}"#);
            serve(workbench, &mut link);
            let source = &workbench.sources[music];
            let expected = (crate::map_link::TOWER_LOUD_SPL_DB - source.declared_spl_at_one_meter_db)
                .clamp(before, MAX_SOURCE_OFFSET_DB);
            assert_eq!(source.monitor_offset_db, expected);
            assert!(expected >= before + 20.0, "really, really loud: +{expected} dB");
            assert!((source.position.up_m - sears["up_m"].as_f64().unwrap() as f32).abs() < 0.5);
            assert!(source.position.up_m > 440.0, "{:?}", source.position);
            let observed = workbench.playback_status_reader.read();
            let loud_state = workbench.map_state_json(&observed, -120.0);
            let loud: serde_json::Value = serde_json::from_str(&loud_state).unwrap();
            assert_eq!(loud["sources"][music]["boost_db"].as_f64(), Some(f64::from(expected - before)));
            std::fs::write(link_dir.join("map-state-loud.json"), &loud_state).unwrap();
            send(
                &serde_json::json!({
                    "type": "move", "id": "music",
                    "east_m": moved.east_m, "north_m": moved.north_m, "done": true,
                })
                .to_string(),
            );
            serve(workbench, &mut link);
            assert_eq!(workbench.sources[music].monitor_offset_db, before, "boost put back");
            std::fs::write(
                link_dir.join("link-proof-loud.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "spot": sears,
                    "trim_db_before": before,
                    "trim_db_at_spot": expected,
                    "trim_db_after_drag": workbench.sources[music].monitor_offset_db,
                    "music_up_m_at_spot": loud["sources"][music]["position_m"][2],
                    "music_level_db_here_at_spot": loud["sources"][music]["level_db"],
                }))
                .unwrap(),
            )
            .unwrap();
        }

        // Music: the song's own band track, its routed field over the
        // walkable dots and its paths to You. A tap on the map then moves You:
        // refused inside a building, snapped onto the street otherwise.
        {
            // Back on the street corner (a drag keeps the tower-top height).
            send(&serde_json::json!({"type": "spot", "id": "music", "key": corner["key"]}).to_string());
            serve(workbench, &mut link);
            assert!(workbench.sources[music].position.up_m < 10.0, "{:?}", workbench.sources[music].position);
            workbench.map.last_state.clear();
            serve(workbench, &mut link);
            read("state");
            let take = |kind: &str| {
                stash
                    .borrow()
                    .get(kind)
                    .cloned()
                    .unwrap_or_else(|| panic!("no {kind} line"))
            };
            let (track_line, field_line) = (take("track"), take("field"));
            let track: serde_json::Value = serde_json::from_str(&track_line).unwrap();
            let field: serde_json::Value = serde_json::from_str(&field_line).unwrap();
            assert_eq!(track["id"], "music");
            let kicks = track["track"]["kicks_s"].as_array().unwrap().len();
            let length_s = track["track"]["length_s"].as_f64().unwrap();
            assert!(length_s > 10.0 && kicks > 4, "{length_s} s, {kicks} kicks");
            assert_eq!(field["id"], "music");
            let source = [
                field["source_m"][0].as_f64().unwrap(),
                field["source_m"][1].as_f64().unwrap(),
            ];
            assert!((source[0] - f64::from(moved.east_m)).abs() < 0.1, "field follows the speaker");
            let field_dots = field["dots"].as_array().unwrap();
            assert!(field_dots.len() > 300, "{} field dots", field_dots.len());
            // Somewhere heard around a corner, about 110 m by street.
            let value = |dot: &serde_json::Value, slot: usize| dot[slot].as_f64().unwrap();
            let mut candidates = field_dots
                .iter()
                .filter(|dot| {
                    let straight = (value(dot, 0) - source[0]).hypot(value(dot, 1) - source[1]);
                    let route = value(dot, 5);
                    route > straight * 1.12 + 4.0 && (60.0..=180.0).contains(&route)
                })
                .collect::<Vec<_>>();
            candidates.sort_by_key(|dot| ((value(dot, 5) - 110.0).abs() * 10.0) as i64);
            // The first whose street spot the planner also reaches (a spot
            // under the elevated tracks, say, has no route to plan).
            let access = workbench
                .walk
                .atlas
                .streets
                .iter()
                .map(|street| street.points.clone())
                .collect::<Vec<_>>();
            let speaker = workbench.feed_planner.route_field(workbench.sources[music].position);
            let around = *candidates
                .iter()
                .find(|dot| {
                    let tap = [value(dot, 0) as f32 + 2.5, value(dot, 1) as f32 + 2.5];
                    crate::map_link::you_landing(tap, &access, &workbench.ground_map.roofs, &workbench.map.footprints)
                        .is_ok_and(|at| {
                            let ear = EnuVector3::new(at[0], at[1], workbench.listener.position.up_m);
                            let plan = workbench.feed_planner.plan(&speaker, ear);
                            !plan.line_of_sight
                                && (plan.primary_route_m.is_some()
                                    || workbench.feed_planner.route_to(&speaker, ear).is_some_and(|route| route.len() > 2))
                        })
                })
                .unwrap_or_else(|| {
                    let landed = candidates
                        .iter()
                        .filter_map(|dot| {
                            let tap = [value(dot, 0) as f32 + 2.5, value(dot, 1) as f32 + 2.5];
                            crate::map_link::you_landing(tap, &access, &workbench.ground_map.roofs, &workbench.map.footprints)
                                .ok()
                        })
                        .collect::<Vec<_>>();
                    let ears = landed
                        .iter()
                        .map(|at| EnuVector3::new(at[0], at[1], workbench.listener.position.up_m))
                        .collect::<Vec<_>>();
                    let hidden = ears.iter().filter(|ear| !workbench.feed_planner.plan(&speaker, **ear).line_of_sight).count();
                    let routed = ears.iter().filter(|ear| workbench.feed_planner.route_to(&speaker, **ear).is_some()).count();
                    panic!(
                        "no street spot around a corner: {} candidates, {} land, {hidden} hidden, {routed} routed; speaker {:?} field at {source:?}",
                        candidates.len(),
                        landed.len(),
                        workbench.sources[music].position,
                    )
                });
            // Bass wraps the corner better than the highs do.
            assert!(value(around, 2) - value(around, 4) > 6.0, "{around}");
            let before = workbench.listener.position;
            let inside = workbench
                .map
                .footprints
                .iter()
                .find_map(|ring| {
                    let n = ring.len() as f32;
                    let centre = [
                        ring.iter().map(|p| p[0]).sum::<f32>() / n,
                        ring.iter().map(|p| p[1]).sum::<f32>() / n,
                    ];
                    crate::map_link::inside_ring(centre, ring).then_some(centre)
                })
                .unwrap();
            send(&serde_json::json!({"type": "move_you", "east_m": inside[0], "north_m": inside[1]}).to_string());
            serve(workbench, &mut link);
            let (_, refused) = read("notice");
            assert!(refused["text"].as_str().unwrap().contains("inside a building"), "{refused}");
            assert_eq!(workbench.listener.position, before);
            let tap = [value(around, 0) as f32 + 2.5, value(around, 1) as f32 + 2.5];
            send(&serde_json::json!({"type": "move_you", "east_m": tap[0], "north_m": tap[1]}).to_string());
            workbench.map.last_paths = None;
            serve(workbench, &mut link);
            let you = workbench.listener.position;
            assert_ne!(you, before, "You moved");
            assert_eq!(you.up_m, before.up_m);
            let snapped = (you.east_m - tap[0]).hypot(you.north_m - tap[1]);
            assert!(snapped <= crate::map_link::YOU_SNAP_M, "{snapped} m");
            let on_street = workbench
                .walk
                .atlas
                .streets
                .iter()
                .flat_map(|street| street.points.windows(2).map(|pair| (pair[0], pair[1])).collect::<Vec<_>>())
                .map(|(a, b)| crate::map_link::segment_distance([you.east_m, you.north_m], a, b))
                .fold(f32::INFINITY, f32::min);
            assert!(on_street < 0.1, "snapped onto a street centreline: {on_street} m");
            let (paths_line, paths) = read("paths");
            assert_eq!(paths["id"], "music");
            assert!((paths["listener_m"][0].as_f64().unwrap() - f64::from(you.east_m)).abs() < 0.1);
            let primary_m = paths["primary"]["length_m"].as_f64().unwrap();
            let echoes = paths["echoes"].as_array().unwrap().len();
            assert!(primary_m > 20.0 && paths["line_of_sight"] == false, "{paths}");
            workbench.map.last_state.clear();
            serve(workbench, &mut link);
            let (music_state_line, music_state) = read("state");
            assert!((music_state["listener"]["position_m"][0].as_f64().unwrap() - f64::from(you.east_m)).abs() < 0.01);
            assert!(music_state["sources"][music]["playhead_s"].is_number());
            std::fs::write(link_dir.join("map-music-track.json"), &track_line).unwrap();
            std::fs::write(link_dir.join("map-music-field.json"), &field_line).unwrap();
            std::fs::write(link_dir.join("map-music-paths.json"), &paths_line).unwrap();
            std::fs::write(link_dir.join("map-state-music.json"), &music_state_line).unwrap();
            std::fs::write(
                link_dir.join("music-proof.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "track_length_s": length_s,
                    "kicks": kicks,
                    "field_dots": field_dots.len(),
                    "field_bytes": field_line.len(),
                    "track_bytes": track_line.len(),
                    "tap_inside_building": inside,
                    "tap_refused": refused["text"],
                    "tap": tap,
                    "you_from": [before.east_m, before.north_m],
                    "you_to": [you.east_m, you.north_m],
                    "snapped_m": snapped,
                    "paths_line_of_sight": paths["line_of_sight"],
                    "paths_primary_m": primary_m,
                    "paths_echoes": echoes,
                }))
                .unwrap(),
            )
            .unwrap();
            println!(
                "[city-map] music: {length_s:.1} s, {kicks} kicks, {} field dots; You {:?} -> {:?}; primary {primary_m} m, {echoes} echoes",
                field_dots.len(),
                [before.east_m, before.north_m],
                [you.east_m, you.north_m],
            );
        }

        // A shot, end to end: the map's Fire resolves (refused headless, as
        // the Workbench's own Play is), then the same arm the Workbench runs
        // on Play plans the artillery shot, and the feed event it publishes
        // reaches the map as one `shot` line. Nothing plays.
        if let Some(index) = workbench.sources.iter().position(|source| source.id == "artillery") {
            send(r#"{"type":"fire","id":"artillery"}"#);
            serve(workbench, &mut link);
            let (_, refused) = read("notice");
            assert_eq!(refused["text"], "Audio isn't ready to play yet", "{refused}");
            workbench.arm_source_play(index, SourceComparisonMode::Spatial);
            let event = {
                let source = &workbench.sources[index];
                let listener = workbench.listener.position;
                let published = workbench
                    .host_echoes
                    .as_ref()
                    .and_then(|echoes| echoes.published_plan(index));
                let fallback;
                let plan = match published {
                    Some(plan) => plan,
                    None => {
                        let field = workbench.feed_planner.route_field(source.position);
                        fallback = workbench.feed_planner.plan(&field, listener);
                        &fallback
                    }
                };
                let delay = source.retrigger_start_delay.map_or(0, |(_, frames)| frames);
                let emission_s =
                    (u64::from(delay) + source.onset_frames as u64) as f64 / f64::from(SAMPLE_RATE);
                let crack = (delay > 0)
                    .then(|| {
                        workbench
                            .ballistic_cracks
                            .iter()
                            .find(|crack| crack.parent_index == index)
                            .and_then(|crack| crack.feed_crack(listener))
                    })
                    .flatten();
                crate::acoustic_feed::AcousticEvent::from_plan(
                    &source.id,
                    index,
                    1,
                    SAMPLE_RATE,
                    0,
                    source.position,
                    listener,
                    emission_s,
                    plan,
                    published.is_some(),
                    crack,
                )
                .unwrap()
            };
            workbench.feed_events.retain(|old| old.source_index != index);
            workbench.feed_events.push(event);
            serve(workbench, &mut link);
            let (shot_line, shot) = read("shot");
            assert_eq!(shot["event"]["source_id"], "artillery");
            let arrivals = shot["event"]["arrivals"].as_array().unwrap();
            assert!(!arrivals.is_empty());
            let kinds = arrivals.iter().map(|arrival| arrival["kind"].as_str().unwrap()).collect::<Vec<_>>();
            std::fs::write(link_dir.join("map-shot.json"), &shot_line).unwrap();
            println!(
                "[city-map] shot: arrivals {kinds:?}, crack {}, line of sight {}",
                !shot["event"]["crack"].is_null(),
                shot["event"]["line_of_sight"]
            );
            // Sent once: a later state follows, with no second shot before it.
            workbench.sources[index].enabled = false;
            serve(workbench, &mut link);
            let next = loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                if !matches!(value["type"].as_str(), Some("notice" | "paths" | "field" | "track")) {
                    break value;
                }
            };
            assert_eq!(next["type"], "state", "the shot is not repeated");
        }
    }

    /// Opt-in visual proof of the acoustic feed: a recorded replay event drawn by
    /// the real UI at fixed audio times, offscreen, with no device or window.
    #[test]
    #[ignore = "retained street package; set FIGHTBOX_SEEING_REPLAY and FIGHTBOX_LIVE2D_ARTIFACT_DIR"]
    fn seeing_street_offscreen() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let output = PathBuf::from(std::env::var("FIGHTBOX_LIVE2D_ARTIFACT_DIR").unwrap());
        validate_replay_root(&output).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let replay: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::env::var("FIGHTBOX_SEEING_REPLAY").unwrap()).unwrap(),
        )
        .unwrap();
        let event: crate::acoustic_feed::AcousticEvent =
            serde_json::from_value(replay["acoustic_events"][0].clone()).unwrap();
        let evidence = root.parent().unwrap().join("evidence");
        let args = LaunchArgs {
            package: evidence.join("megablock-seed1/megablock.fightbox"),
            baked: evidence
                .join("astra-user-weak-street/road-sample-matrix/successor-path1500-vis40.baked"),
            fixtures: vec![root.join("fixtures/city/astra-artillery/street-path-candidate.json")],
            start_audio: false,
            null_output: false,
            render_out: None,
            render_format: RenderFormat::Binaural,
            live_input_wav: None,
            program_file: None,
            device: None,
            replay: None,
            quiet_audition: None,
        };
        let mut app = WorkbenchApp::load(args, Instant::now()).unwrap();
        let workbench = app.active.as_mut().unwrap();
        assert!(workbench.choose_listening_spot(434.02));
        let (mut status_writer, status_reader) =
            SnapshotPublication::new(PlaybackSnapshot::default());
        workbench.playback_status_reader = status_reader;
        workbench.feed_events = vec![event];
        let ctx = egui::Context::default();
        let mut raster = crate::ground_map::offscreen::Raster::default();
        for (name, map, listening, seconds) in [
            ("map-t2.05-crack", true, false, 2.05),
            ("map-t2.80-ripple", true, false, 2.80),
            ("map-t3.12-routed", true, false, 3.12),
            ("map-t3.45-echoes", true, false, 3.45),
            ("listening-t3.12", false, true, 3.12),
            ("walk-t3.12", false, false, 3.12),
        ] {
            let mut snapshot = PlaybackSnapshot::default();
            snapshot.sources[0] = SourcePlaybackStatus {
                enabled: true,
                observed: true,
                audio_sample: event.trigger_audio_sample
                    + (seconds * f64::from(SAMPLE_RATE)) as u64,
                ..SourcePlaybackStatus::default()
            };
            status_writer.publish(snapshot);
            workbench.listening_mode = listening;
            workbench.ground_map_enabled = map;
            workbench.ground_map_whole_scene = map;
            workbench.anomaly_field.selected_source = 0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 820.0))),
                ..Default::default()
            };
            let frame = ctx.run(input, |ctx| workbench.update_ui(ctx));
            let triangles = raster.save(&ctx, frame, &output.join(format!("{name}.png")));
            assert!(triangles > 100);
            assert!(matches!(workbench.audio, AudioState::Stopped));
        }
    }
}
