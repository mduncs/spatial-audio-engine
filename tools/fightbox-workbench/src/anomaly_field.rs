//! Background proxy-field, persistent live-trail, and adaptive corner sampling.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fightbox_api::{EnuVector3 as ApiEnuVector3, Pose};
use fightbox_evidence::sha256_hex;
use fightbox_steam_audio::{
    ADAPTIVE_LIVE_SAMPLE_HZ, AnomalyClass, AnomalyQuerySession, AnomalyRawSample, GridSpec,
    IntersectionTrigger, MultiSourceDescriptor, ProxyCell, S3SimulationConfig, SceneMesh,
    classify_grid, classify_sample_at_distance,
};
use fightbox_world::LoadedPackage;
use serde::{Deserialize, Serialize};

use crate::acoustic_state::{ProbeCoverage, SourceAcousticState};
use crate::fixture::load_baked;

const FIELD_SCHEMA: &str = "fightbox.anomaly-field.v1";
const TRAIL_SCHEMA: &str = "fightbox.anomaly-trail.v1";
const CLASSIFIER_SCHEMA: u32 = 2;
const ADAPTIVE_RADIUS_M: f32 = 3.0;
const ADAPTIVE_SPACING_M: f32 = 2.0;
const SOURCE_COVERED_BIT: u32 = 1 << 31;
const LISTENER_COVERED_BIT: u32 = 1 << 30;
const ANOMALY_BITS: u32 = (1 << AnomalyClass::ALL.len()) - 1;

#[derive(Clone)]
pub(crate) struct FieldContext {
    package: LoadedPackage,
    scene: SceneMesh,
    baked_path: PathBuf,
    simulation: S3SimulationConfig,
    bounds: [[f32; 2]; 2],
    fixture_hash: String,
    mesh_hash: String,
    materials_hash: String,
    bake_hash: String,
    engine_key: String,
    cache_root: PathBuf,
}

impl FieldContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        package: LoadedPackage,
        scene: SceneMesh,
        baked_path: PathBuf,
        simulation: S3SimulationConfig,
        bounds: [[f32; 2]; 2],
        fixture_hash: String,
        bake_hash: String,
        engine_key: String,
        cache_root: PathBuf,
    ) -> Self {
        let mesh_hash = package.manifest.mesh_content_sha256.clone();
        let materials_hash = package.manifest.materials_content_sha256.clone();
        Self {
            package,
            scene,
            baked_path,
            simulation,
            bounds,
            fixture_hash,
            mesh_hash,
            materials_hash,
            bake_hash,
            engine_key,
            cache_root,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SourceQuery {
    pub(crate) id: String,
    pub(crate) position: ApiEnuVector3,
    pub(crate) spl_at_one_meter_db: f32,
    pub(crate) descriptor: MultiSourceDescriptor,
    pub(crate) asset_identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FieldIdentity {
    schema: String,
    mesh_hash: String,
    materials_hash: String,
    bake_hash: String,
    fixture_hash: String,
    source_id: String,
    source_position_bits: [u32; 3],
    source_spl_bits: u32,
    source_descriptor_key: String,
    asset_identity: String,
    grid_bits: [u32; 6],
    simulation_key: String,
}

impl FieldIdentity {
    fn new(context: &FieldContext, source: &SourceQuery, grid: GridSpec) -> Self {
        Self {
            schema: FIELD_SCHEMA.into(),
            mesh_hash: context.mesh_hash.clone(),
            materials_hash: context.materials_hash.clone(),
            bake_hash: context.bake_hash.clone(),
            fixture_hash: context.fixture_hash.clone(),
            source_id: source.id.clone(),
            source_position_bits: [
                source.position.east_m.to_bits(),
                source.position.north_m.to_bits(),
                source.position.up_m.to_bits(),
            ],
            source_spl_bits: source.spl_at_one_meter_db.to_bits(),
            source_descriptor_key: format!("{:?}", source.descriptor),
            asset_identity: source.asset_identity.clone(),
            grid_bits: [
                grid.min_enu[0].to_bits(),
                grid.min_enu[1].to_bits(),
                grid.max_enu[0].to_bits(),
                grid.max_enu[1].to_bits(),
                grid.listener_height_m.to_bits(),
                grid.spacing_m.to_bits(),
            ],
            simulation_key: format!(
                "engine={}:steam={}/{}:hrtf=steam-default:ray-v1:path-M{}:samples={}:radius={:08x}:threshold={:08x}:range={:08x}:validate={}:alternate={}:schema={CLASSIFIER_SCHEMA}",
                context.engine_key,
                fightbox_steam_audio::STEAM_AUDIO_VERSION,
                fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT,
                context.simulation.pathing_order,
                context.simulation.pathing_visibility_samples,
                context.simulation.pathing_visibility_radius_m.to_bits(),
                context.simulation.pathing_visibility_threshold.to_bits(),
                context.simulation.pathing_visibility_range_m.to_bits(),
                context.simulation.validate_paths,
                context.simulation.find_alternate_paths,
            ),
        }
    }

    fn slug(&self) -> String {
        let json = serde_json::to_vec(self).expect("field identity is serializable");
        format!(
            "{}-{}",
            safe_name(&self.source_id),
            &sha256_hex(&json)[..16]
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FieldLayer {
    pub(crate) identity: FieldIdentity,
    pub(crate) grid: GridSpec,
    pub(crate) cells: Vec<ProxyCell>,
}

impl FieldLayer {
    pub(crate) fn is_stale(&self, current: &FieldIdentity) -> bool {
        &self.identity != current
    }
}

enum SweepEvent {
    Progress { complete: usize, total: usize },
    Complete(FieldLayer),
    Failed(String),
    Cancelled,
}

struct SweepWorker {
    cancel: Arc<AtomicBool>,
    events: Receiver<SweepEvent>,
}

enum AdaptiveCommand {
    Area([f32; 3]),
}

enum AdaptiveEvent {
    Ready,
    Sample(ProxyCell),
    Failed(String),
}

struct AdaptiveWorker {
    identity: FieldIdentity,
    cancel: Arc<AtomicBool>,
    commands: SyncSender<AdaptiveCommand>,
    events: Receiver<AdaptiveEvent>,
}

struct TrailWriter {
    identity: FieldIdentity,
    sender: SyncSender<StoredTrailSample>,
    loaded: Receiver<Result<Vec<ProxyCell>, String>>,
}

#[derive(Clone, Debug)]
pub(crate) struct TrailLayer {
    pub(crate) identity: FieldIdentity,
    pub(crate) cells: Vec<ProxyCell>,
    pub(crate) adaptive_cells: usize,
}

pub(crate) struct FieldController {
    context: FieldContext,
    pub(crate) overlay_enabled: bool,
    pub(crate) trail_enabled: bool,
    pub(crate) adaptive_enabled: bool,
    pub(crate) spacing_m: f32,
    pub(crate) selected_source: usize,
    pub(crate) status: String,
    pub(crate) field: Option<FieldLayer>,
    pub(crate) trail: Option<TrailLayer>,
    sweep: Option<SweepWorker>,
    adaptive: Option<AdaptiveWorker>,
    trail_writer: Option<TrailWriter>,
    trigger: IntersectionTrigger,
    last_live_sample: Option<Instant>,
    last_live_energy_sequence: u64,
    stale_reason: Option<String>,
}

impl FieldController {
    pub(crate) fn new(context: FieldContext) -> Self {
        Self {
            context,
            overlay_enabled: true,
            trail_enabled: true,
            adaptive_enabled: true,
            spacing_m: 8.0,
            selected_source: 0,
            status: "Proxy field not run · live trail armed".into(),
            field: None,
            trail: None,
            sweep: None,
            adaptive: None,
            trail_writer: None,
            trigger: IntersectionTrigger::default(),
            last_live_sample: None,
            last_live_energy_sequence: 0,
            stale_reason: None,
        }
    }

    pub(crate) fn grid(&self, listener_height_m: f32) -> GridSpec {
        GridSpec {
            min_enu: self.context.bounds[0],
            max_enu: self.context.bounds[1],
            listener_height_m,
            spacing_m: self.spacing_m,
        }
    }

    pub(crate) fn identity(&self, source: &SourceQuery, listener_height_m: f32) -> FieldIdentity {
        FieldIdentity::new(&self.context, source, self.grid(listener_height_m))
    }

    pub(crate) fn start_sweep(&mut self, source: SourceQuery, listener_height_m: f32) {
        self.cancel_sweep();
        let grid = self.grid(listener_height_m);
        let identity = FieldIdentity::new(&self.context, &source, grid);
        let (sender, events) = sync_channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let context = self.context.clone();
        thread::Builder::new()
            .name("anomaly-field-sweep".into())
            .spawn(move || run_sweep(context, source, identity, grid, worker_cancel, sender))
            .expect("anomaly field worker thread creation should succeed");
        self.sweep = Some(SweepWorker { cancel, events });
        self.stale_reason = None;
        self.status = format!("Loading query-only session · {} cells", grid.cell_count());
    }

    pub(crate) fn cancel_sweep(&mut self) {
        if let Some(worker) = self.sweep.take() {
            worker.cancel.store(true, Ordering::Release);
        }
    }

    pub(crate) fn invalidate(&mut self, reason: impl Into<String>) {
        self.cancel_sweep();
        if let Some(worker) = self.adaptive.take() {
            worker.cancel.store(true, Ordering::Release);
        }
        self.trail_writer = None;
        self.trigger = IntersectionTrigger::default();
        self.last_live_sample = None;
        self.last_live_energy_sequence = 0;
        let reason = reason.into();
        self.stale_reason = Some(reason.clone());
        self.status = format!("STALE · {reason}");
    }

    pub(crate) fn stale_reason(&self) -> Option<&str> {
        self.stale_reason.as_deref()
    }

    pub(crate) fn poll(&mut self) {
        let mut finished = false;
        if let Some(worker) = &self.sweep {
            loop {
                match worker.events.try_recv() {
                    Ok(SweepEvent::Progress { complete, total }) => {
                        self.status = format!(
                            "Proxy sweep {complete}/{total} · {:.0}%",
                            complete as f32 * 100.0 / total as f32
                        );
                    }
                    Ok(SweepEvent::Complete(layer)) => {
                        self.status = format!(
                            "SHADOW + WEAK PATH · {} cells · {} flagged",
                            layer.cells.len(),
                            layer
                                .cells
                                .iter()
                                .filter(|cell| !cell.flags.is_empty())
                                .count()
                        );
                        self.field = Some(layer);
                        finished = true;
                    }
                    Ok(SweepEvent::Failed(error)) => {
                        self.status = format!("Proxy sweep failed · {error}");
                        finished = true;
                    }
                    Ok(SweepEvent::Cancelled) => {
                        self.status = "Proxy sweep cancelled".into();
                        finished = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.sweep = None;
        }
        self.poll_trail_load();
        self.poll_adaptive();
    }

    pub(crate) fn observe_live(
        &mut self,
        now: Instant,
        listener: ApiEnuVector3,
        source: SourceQuery,
        acoustic: SourceAcousticState,
        energy: fightbox_steam_audio::LiveStageEnergySnapshot,
    ) {
        if !self.trail_enabled
            || energy.sequence == 0
            || energy.sequence == self.last_live_energy_sequence
            || self
                .last_live_sample
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(200))
        {
            return;
        }
        let (Some(direct_audibility), Some(path_eq), Some(path_sh_energy)) = (
            acoustic.occlusion,
            acoustic.path_eq,
            acoustic.path_sh_energy,
        ) else {
            return;
        };
        let mut identity = self.identity(&source, listener.up_m);
        identity.simulation_key.push_str(&format!(
            ":live-world={}:quality={:?}",
            energy.world_generation, acoustic.quality
        ));
        self.ensure_trail(identity.clone());
        let raw = AnomalyRawSample {
            position_enu: fightbox_steam_audio::EnuVector3::new(
                listener.east_m,
                listener.north_m,
                listener.up_m,
            ),
            direct_audibility,
            path_eq,
            path_sh_energy,
            path_coefficient_min: path_eq.into_iter().fold(f32::INFINITY, f32::min),
            path_coefficient_max: path_eq.into_iter().fold(f32::NEG_INFINITY, f32::max),
            source_probe_covered: acoustic.source_probes == ProbeCoverage::Covered,
            listener_probe_covered: acoustic.listener_probes == ProbeCoverage::Covered,
            // Live poses come from the operator/trajectory rather than a synthetic
            // field grid. Query-only sweeps perform the static-solid endpoint test.
            source_endpoint_inside_static_geometry: false,
            listener_endpoint_inside_static_geometry: false,
            direct_path_energy: (energy.audible_source_count == 1)
                .then_some(energy.direct_path_energy),
            reflection_energy: (energy.audible_source_count == 1)
                .then_some(energy.reflection_energy),
        };
        let cell = classify_sample_at_distance(
            raw,
            source.spl_at_one_meter_db,
            api_distance(source.position, listener),
        );
        self.last_live_sample = Some(now);
        self.last_live_energy_sequence = energy.sequence;
        if let Some(trail) = &mut self.trail {
            trail.cells.push(cell);
        }
        if let Some(writer) = &self.trail_writer {
            let _ = writer
                .sender
                .try_send(StoredTrailSample::from_cell(cell, false));
        }
        if self.adaptive_enabled
            && self
                .trigger
                .observe([listener.east_m, listener.north_m], direct_audibility)
        {
            self.request_adaptive(
                source,
                identity,
                [listener.east_m, listener.north_m, listener.up_m],
            );
        }
    }

    fn ensure_trail(&mut self, identity: FieldIdentity) {
        if self
            .trail
            .as_ref()
            .is_some_and(|trail| trail.identity == identity)
        {
            return;
        }
        self.trail = Some(TrailLayer {
            identity: identity.clone(),
            cells: Vec::new(),
            adaptive_cells: 0,
        });
        let (sender, receiver) = sync_channel(64);
        let (loaded_sender, loaded) = sync_channel(1);
        let path = self
            .context
            .cache_root
            .join("trails")
            .join(format!("{}.jsonl", identity.slug()));
        let writer_identity = identity.clone();
        thread::Builder::new()
            .name("anomaly-trail-writer".into())
            .spawn(move || run_trail_writer(path, receiver, loaded_sender))
            .expect("trail writer thread creation should succeed");
        self.trail_writer = Some(TrailWriter {
            identity: writer_identity,
            sender,
            loaded,
        });
    }

    fn poll_trail_load(&mut self) {
        let Some(writer) = &self.trail_writer else {
            return;
        };
        let Ok(result) = writer.loaded.try_recv() else {
            return;
        };
        match result {
            Ok(mut loaded) => {
                if let Some(trail) = &mut self.trail
                    && trail.identity == writer.identity
                {
                    loaded.append(&mut trail.cells);
                    trail.cells = loaded;
                }
            }
            Err(error) => self.status = format!("Trail restore warning · {error}"),
        }
    }

    fn request_adaptive(&mut self, source: SourceQuery, identity: FieldIdentity, center: [f32; 3]) {
        if self
            .adaptive
            .as_ref()
            .is_none_or(|worker| worker.identity != identity)
        {
            if let Some(old) = self.adaptive.take() {
                old.cancel.store(true, Ordering::Release);
            }
            let (commands, receiver) = sync_channel(4);
            let (sender, events) = sync_channel(16);
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = Arc::clone(&cancel);
            let context = self.context.clone();
            let worker_identity = identity.clone();
            thread::Builder::new()
                .name("anomaly-adaptive-sampler".into())
                .spawn(move || run_adaptive(context, source, receiver, sender, worker_cancel))
                .expect("adaptive sampler thread creation should succeed");
            self.adaptive = Some(AdaptiveWorker {
                identity: worker_identity,
                cancel,
                commands,
                events,
            });
            self.status = "Intersection transition · loading bounded adaptive sampler".into();
        }
        if let Some(worker) = &self.adaptive {
            let _ = worker.commands.try_send(AdaptiveCommand::Area(center));
        }
    }

    fn poll_adaptive(&mut self) {
        let Some(worker) = &self.adaptive else {
            return;
        };
        loop {
            match worker.events.try_recv() {
                Ok(AdaptiveEvent::Ready) => {
                    self.status = format!(
                        "Adaptive sampler ready · ≤{} samples/s",
                        ADAPTIVE_LIVE_SAMPLE_HZ
                    );
                }
                Ok(AdaptiveEvent::Sample(cell)) => {
                    if let Some(trail) = &mut self.trail
                        && trail.identity == worker.identity
                    {
                        trail.cells.push(cell);
                        trail.adaptive_cells += 1;
                    }
                    if let Some(writer) = &self.trail_writer
                        && writer.identity == worker.identity
                    {
                        let _ = writer
                            .sender
                            .try_send(StoredTrailSample::from_cell(cell, true));
                    }
                }
                Ok(AdaptiveEvent::Failed(error)) => {
                    self.status = format!("Adaptive sampler failed · {error}");
                    break;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }
}

impl Drop for FieldController {
    fn drop(&mut self) {
        self.cancel_sweep();
        if let Some(worker) = self.adaptive.take() {
            worker.cancel.store(true, Ordering::Release);
        }
    }
}

fn run_sweep(
    context: FieldContext,
    source: SourceQuery,
    identity: FieldIdentity,
    grid: GridSpec,
    cancel: Arc<AtomicBool>,
    events: SyncSender<SweepEvent>,
) {
    let result = (|| -> Result<FieldLayer, String> {
        if let Some(layer) = restore_field(&context.cache_root, &identity, grid)? {
            return Ok(layer);
        }
        let baked = load_baked(&context.baked_path, &context.package)?;
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        let mut query = AnomalyQuerySession::new(
            &context.scene,
            &baked,
            context.simulation,
            source.descriptor,
        )
        .map_err(|error| error.to_string())?;
        let mut cells = Vec::with_capacity(grid.cell_count());
        let stride = grid.width().saturating_mul(4).max(1);
        for index in 0..grid.cell_count() {
            if cancel.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            let position = grid.position(index);
            let raw = query.sample(position).map_err(|error| error.to_string())?;
            cells.push(classify_sample_at_distance(
                raw,
                source.spl_at_one_meter_db,
                api_backend_distance(source.position, position),
            ));
            if (index + 1).is_multiple_of(stride) || index + 1 == grid.cell_count() {
                let _ = events.try_send(SweepEvent::Progress {
                    complete: index + 1,
                    total: grid.cell_count(),
                });
            }
        }
        classify_grid(&mut cells, grid.width(), grid.height(), grid.spacing_m);
        let layer = FieldLayer {
            identity,
            grid,
            cells,
        };
        persist_field(&context.cache_root, &layer)?;
        Ok(layer)
    })();
    match result {
        Ok(layer) => {
            let _ = events.send(SweepEvent::Complete(layer));
        }
        Err(error) if error == "cancelled" => {
            let _ = events.send(SweepEvent::Cancelled);
        }
        Err(error) => {
            let _ = events.send(SweepEvent::Failed(error));
        }
    }
}

fn run_adaptive(
    context: FieldContext,
    source: SourceQuery,
    commands: Receiver<AdaptiveCommand>,
    events: SyncSender<AdaptiveEvent>,
    cancel: Arc<AtomicBool>,
) {
    let result = (|| -> Result<(), String> {
        let baked = load_baked(&context.baked_path, &context.package)?;
        let mut query = AnomalyQuerySession::new(
            &context.scene,
            &baked,
            context.simulation,
            source.descriptor,
        )
        .map_err(|error| error.to_string())?;
        let _ = events.try_send(AdaptiveEvent::Ready);
        while let Ok(AdaptiveCommand::Area(center)) = commands.recv() {
            for offset in adaptive_offsets() {
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                let position = fightbox_steam_audio::EnuVector3::new(
                    (center[0] + offset[0]).clamp(context.bounds[0][0], context.bounds[1][0]),
                    (center[1] + offset[1]).clamp(context.bounds[0][1], context.bounds[1][1]),
                    center[2],
                );
                let raw = query.sample(position).map_err(|error| error.to_string())?;
                let cell = classify_sample_at_distance(
                    raw,
                    source.spl_at_one_meter_db,
                    api_backend_distance(source.position, position),
                );
                let _ = events.send(AdaptiveEvent::Sample(cell));
                thread::sleep(Duration::from_millis(
                    1_000 / u64::from(ADAPTIVE_LIVE_SAMPLE_HZ),
                ));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = events.send(AdaptiveEvent::Failed(error));
    }
}

fn adaptive_offsets() -> [[f32; 2]; 8] {
    let diagonal = ADAPTIVE_RADIUS_M / 2.0_f32.sqrt();
    [
        [-ADAPTIVE_SPACING_M, 0.0],
        [ADAPTIVE_SPACING_M, 0.0],
        [0.0, -ADAPTIVE_SPACING_M],
        [0.0, ADAPTIVE_SPACING_M],
        [-diagonal, -diagonal],
        [-diagonal, diagonal],
        [diagonal, -diagonal],
        [diagonal, diagonal],
    ]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredTrailSample {
    schema: String,
    position: [f32; 3],
    direct_audibility: f32,
    direct_loss_db: f32,
    path_sh_energy: f32,
    path_strength_db: f32,
    free_field_db: f32,
    score: f32,
    source_probe_covered: bool,
    listener_probe_covered: bool,
    flags: u32,
    adaptive: bool,
}

impl StoredTrailSample {
    fn from_cell(cell: ProxyCell, adaptive: bool) -> Self {
        Self {
            schema: TRAIL_SCHEMA.into(),
            position: [
                cell.position_enu.x,
                cell.position_enu.y,
                cell.position_enu.z,
            ],
            direct_audibility: cell.direct_audibility,
            direct_loss_db: cell.direct_loss_db,
            path_sh_energy: cell.path_sh_energy,
            path_strength_db: cell.path_strength_db,
            free_field_db: cell.free_field_db,
            score: cell.score,
            source_probe_covered: cell.source_probe_covered,
            listener_probe_covered: cell.listener_probe_covered,
            flags: cell.flags.0,
            adaptive,
        }
    }

    fn into_cell(self) -> ProxyCell {
        ProxyCell {
            position_enu: fightbox_steam_audio::EnuVector3::new(
                self.position[0],
                self.position[1],
                self.position[2],
            ),
            direct_audibility: self.direct_audibility,
            direct_loss_db: self.direct_loss_db,
            path_sh_energy: self.path_sh_energy,
            path_strength_db: self.path_strength_db,
            free_field_db: self.free_field_db,
            score: self.score,
            source_probe_covered: self.source_probe_covered,
            listener_probe_covered: self.listener_probe_covered,
            direct_path_energy: None,
            reflection_energy: None,
            reflection_excess_db: None,
            flags: fightbox_steam_audio::AnomalyFlags(self.flags),
        }
    }
}

fn run_trail_writer(
    path: PathBuf,
    receiver: Receiver<StoredTrailSample>,
    loaded: SyncSender<Result<Vec<ProxyCell>, String>>,
) {
    let restored = restore_trail(&path);
    let _ = loaded.send(restored);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    while let Ok(sample) = receiver.recv() {
        if serde_json::to_writer(&mut file, &sample).is_err()
            || file.write_all(b"\n").is_err()
            || file.flush().is_err()
        {
            return;
        }
    }
}

fn restore_trail(path: &PathBuf) -> Result<Vec<ProxyCell>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    BufReader::new(file)
        .lines()
        .map(|line| {
            let line = line.map_err(|error| error.to_string())?;
            serde_json::from_str::<StoredTrailSample>(&line)
                .map(StoredTrailSample::into_cell)
                .map_err(|error| error.to_string())
        })
        .collect()
}

#[derive(Serialize, Deserialize)]
struct CachedManifest {
    schema: String,
    identity: FieldIdentity,
    width: usize,
    height: usize,
    spacing_m: f32,
    listener_height_m: f32,
    cell_count: usize,
    anomaly_counts: Vec<(String, usize)>,
    raster_file: String,
}

fn persist_field(root: &PathBuf, layer: &FieldLayer) -> Result<(), String> {
    let directory = root.join("fields").join(layer.identity.slug());
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let raster = encode_raster(layer);
    write_replace(&directory.join("cells.bin"), &raster)?;
    let manifest = CachedManifest {
        schema: FIELD_SCHEMA.into(),
        identity: layer.identity.clone(),
        width: layer.grid.width(),
        height: layer.grid.height(),
        spacing_m: layer.grid.spacing_m,
        listener_height_m: layer.grid.listener_height_m,
        cell_count: layer.cells.len(),
        anomaly_counts: AnomalyClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class.id().into(),
                    layer
                        .cells
                        .iter()
                        .filter(|cell| cell.flags.contains(class))
                        .count(),
                )
            })
            .collect(),
        raster_file: "cells.bin".into(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    write_replace(&directory.join("manifest.json"), &bytes)
}

fn write_replace(path: &PathBuf, bytes: &[u8]) -> Result<(), String> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{suffix}"));
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn encode_raster(layer: &FieldLayer) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(24 + layer.cells.len() * 40);
    bytes.extend_from_slice(b"FBXANOM\0");
    bytes.extend_from_slice(&(layer.grid.width() as u32).to_le_bytes());
    bytes.extend_from_slice(&(layer.grid.height() as u32).to_le_bytes());
    bytes.extend_from_slice(&layer.grid.spacing_m.to_le_bytes());
    bytes.extend_from_slice(&layer.grid.listener_height_m.to_le_bytes());
    for cell in &layer.cells {
        for value in [
            cell.position_enu.x,
            cell.position_enu.y,
            cell.position_enu.z,
            cell.direct_audibility,
            cell.direct_loss_db,
            cell.path_sh_energy,
            cell.path_strength_db,
            cell.free_field_db,
            cell.score,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let mut stored_flags = cell.flags.0 & ANOMALY_BITS;
        if cell.source_probe_covered {
            stored_flags |= SOURCE_COVERED_BIT;
        }
        if cell.listener_probe_covered {
            stored_flags |= LISTENER_COVERED_BIT;
        }
        bytes.extend_from_slice(&stored_flags.to_le_bytes());
    }
    bytes
}

fn restore_field(
    root: &std::path::Path,
    identity: &FieldIdentity,
    grid: GridSpec,
) -> Result<Option<FieldLayer>, String> {
    let directory = root.join("fields").join(identity.slug());
    let manifest_path = directory.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|error| error.to_string())?;
    let manifest: CachedManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| error.to_string())?;
    if manifest.schema != FIELD_SCHEMA
        || &manifest.identity != identity
        || manifest.width != grid.width()
        || manifest.height != grid.height()
        || manifest.spacing_m.to_bits() != grid.spacing_m.to_bits()
        || manifest.listener_height_m.to_bits() != grid.listener_height_m.to_bits()
        || manifest.cell_count != grid.cell_count()
        || manifest.raster_file != "cells.bin"
    {
        return Err("cached anomaly field identity or grid does not match its directory".into());
    }
    let bytes =
        std::fs::read(directory.join(&manifest.raster_file)).map_err(|error| error.to_string())?;
    let cells = decode_raster(grid, &bytes)?;
    Ok(Some(FieldLayer {
        identity: identity.clone(),
        grid,
        cells,
    }))
}

fn decode_raster(grid: GridSpec, bytes: &[u8]) -> Result<Vec<ProxyCell>, String> {
    let expected = 24_usize
        .checked_add(
            grid.cell_count()
                .checked_mul(40)
                .ok_or_else(|| "cached anomaly raster length overflow".to_string())?,
        )
        .ok_or_else(|| "cached anomaly raster length overflow".to_string())?;
    if bytes.len() != expected || bytes.get(..8) != Some(b"FBXANOM\0") {
        return Err("cached anomaly raster has the wrong magic or byte length".into());
    }
    let width = read_u32(bytes, 8)? as usize;
    let height = read_u32(bytes, 12)? as usize;
    let spacing_m = read_f32(bytes, 16)?;
    let listener_height_m = read_f32(bytes, 20)?;
    if width != grid.width()
        || height != grid.height()
        || spacing_m.to_bits() != grid.spacing_m.to_bits()
        || listener_height_m.to_bits() != grid.listener_height_m.to_bits()
    {
        return Err("cached anomaly raster header does not match its manifest".into());
    }
    let mut cells = Vec::with_capacity(grid.cell_count());
    for index in 0..grid.cell_count() {
        let offset = 24 + index * 40;
        let values = [
            read_f32(bytes, offset)?,
            read_f32(bytes, offset + 4)?,
            read_f32(bytes, offset + 8)?,
            read_f32(bytes, offset + 12)?,
            read_f32(bytes, offset + 16)?,
            read_f32(bytes, offset + 20)?,
            read_f32(bytes, offset + 24)?,
            read_f32(bytes, offset + 28)?,
            read_f32(bytes, offset + 32)?,
        ];
        let stored_flags = read_u32(bytes, offset + 36)?;
        cells.push(ProxyCell {
            position_enu: fightbox_steam_audio::EnuVector3::new(values[0], values[1], values[2]),
            direct_audibility: values[3],
            direct_loss_db: values[4],
            path_sh_energy: values[5],
            path_strength_db: values[6],
            free_field_db: values[7],
            score: values[8],
            source_probe_covered: stored_flags & SOURCE_COVERED_BIT != 0,
            listener_probe_covered: stored_flags & LISTENER_COVERED_BIT != 0,
            direct_path_energy: None,
            reflection_energy: None,
            reflection_excess_db: None,
            flags: fightbox_steam_audio::AnomalyFlags(stored_flags & ANOMALY_BITS),
        });
    }
    Ok(cells)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| "cached anomaly raster ended early".to_string())
}

fn read_f32(bytes: &[u8], offset: usize) -> Result<f32, String> {
    read_u32(bytes, offset).map(f32::from_bits)
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn api_distance(left: ApiEnuVector3, right: ApiEnuVector3) -> f32 {
    (left.east_m - right.east_m)
        .hypot(left.north_m - right.north_m)
        .hypot(left.up_m - right.up_m)
}

fn api_backend_distance(left: ApiEnuVector3, right: fightbox_steam_audio::EnuVector3) -> f32 {
    (left.east_m - right.x)
        .hypot(left.north_m - right.y)
        .hypot(left.up_m - right.z)
}

pub(crate) fn source_query(
    id: &str,
    position: ApiEnuVector3,
    spl_at_one_meter_db: f32,
    descriptor: MultiSourceDescriptor,
    asset_identity: String,
) -> SourceQuery {
    let descriptor = descriptor.with_initial_pose(Pose {
        position,
        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
    });
    SourceQuery {
        id: id.into(),
        position,
        spl_at_one_meter_db,
        descriptor,
        asset_identity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> FieldIdentity {
        FieldIdentity {
            schema: FIELD_SCHEMA.into(),
            mesh_hash: "mesh".into(),
            materials_hash: "materials".into(),
            bake_hash: "bake".into(),
            fixture_hash: "fixture".into(),
            source_id: "source".into(),
            source_position_bits: [0; 3],
            source_spl_bits: 105.0_f32.to_bits(),
            source_descriptor_key: "point-omni".into(),
            asset_identity: "asset".into(),
            grid_bits: [0; 6],
            simulation_key: "path".into(),
        }
    }

    #[test]
    fn overlay_staleness_invalidates_provenance_and_grid_changes() {
        let original = identity();
        let layer = FieldLayer {
            identity: original.clone(),
            grid: GridSpec {
                min_enu: [0.0; 2],
                max_enu: [8.0; 2],
                listener_height_m: 1.5,
                spacing_m: 8.0,
            },
            cells: Vec::new(),
        };
        assert!(!layer.is_stale(&original));
        for mutate in [
            |identity: &mut FieldIdentity| identity.bake_hash.push('x'),
            |identity: &mut FieldIdentity| identity.source_position_bits[2] ^= 1,
            |identity: &mut FieldIdentity| identity.asset_identity.push('x'),
            |identity: &mut FieldIdentity| identity.grid_bits[5] ^= 1,
            |identity: &mut FieldIdentity| identity.simulation_key.push('x'),
        ] {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert!(layer.is_stale(&changed));
        }
    }

    #[test]
    fn adaptive_pattern_is_bounded_and_local() {
        let offsets = adaptive_offsets();
        assert_eq!(offsets.len(), 8);
        assert!(
            offsets
                .into_iter()
                .all(|offset| offset[0].hypot(offset[1]) <= ADAPTIVE_RADIUS_M + 1.0e-5)
        );
        assert_eq!(ADAPTIVE_LIVE_SAMPLE_HZ, 5);
    }

    #[test]
    fn cached_raster_round_trip_preserves_metrics_flags_and_coverage() {
        let grid = GridSpec {
            min_enu: [0.0; 2],
            max_enu: [8.0; 2],
            listener_height_m: 1.5,
            spacing_m: 8.0,
        };
        let mut flags = fightbox_steam_audio::AnomalyFlags::default();
        flags.insert(AnomalyClass::InvalidCoefficient);
        let cell = ProxyCell {
            position_enu: fightbox_steam_audio::EnuVector3::new(4.0, 4.0, 1.5),
            direct_audibility: 0.25,
            direct_loss_db: 12.0,
            path_sh_energy: 0.125,
            path_strength_db: -18.0,
            free_field_db: 72.0,
            score: 0.75,
            source_probe_covered: true,
            listener_probe_covered: false,
            direct_path_energy: None,
            reflection_energy: None,
            reflection_excess_db: None,
            flags,
        };
        let layer = FieldLayer {
            identity: identity(),
            grid,
            cells: vec![cell],
        };
        let restored = decode_raster(grid, &encode_raster(&layer)).unwrap();
        assert_eq!(restored, vec![cell]);
    }
}
