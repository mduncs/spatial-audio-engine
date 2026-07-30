use std::cell::UnsafeCell;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fightbox_api::EngineConfig;
use fightbox_evidence::{
    CallbackStatus, CallbackTiming, CaptureConfig, CaptureRunManifest, ExplicitClaim,
    ExplicitNonClaim, FixtureId, KernelProvenance, RunProvenance, RunState, SimulationCadence,
    StemRecord, WorldProvenance,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::workbench::StageMix;

pub const CAPTURE_SCHEMA_VERSION: &str = "fightbox.workbench-capture.v1";
pub const MAX_CAPTURE_SECONDS: u64 = 120;
const CAPTURE_BLOCK_FRAMES: usize = 128;
const CAPTURE_CHANNELS: usize = 2;
const CAPTURE_BLOCK_SAMPLES: usize = CAPTURE_BLOCK_FRAMES * CAPTURE_CHANNELS;
const CAPTURE_RING_BLOCKS: usize = 4_096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureSourceState {
    pub id: String,
    pub asset_id: String,
    pub enabled: bool,
    pub muted: bool,
    pub soloed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureStageState {
    pub direct: StageState,
    pub pathing: StageState,
    pub reflections: StageState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageState {
    pub bypassed: bool,
    pub soloed: bool,
    pub output_enabled: bool,
}

impl From<StageMix> for CaptureStageState {
    fn from(mix: StageMix) -> Self {
        let gains = mix.gains();
        Self {
            direct: StageState {
                bypassed: mix.bypassed[0],
                soloed: mix.soloed[0],
                output_enabled: gains.direct != 0.0,
            },
            pathing: StageState {
                bypassed: mix.bypassed[1],
                soloed: mix.soloed[1],
                output_enabled: gains.pathing != 0.0,
            },
            reflections: StageState {
                bypassed: mix.bypassed[2],
                soloed: mix.soloed[2],
                output_enabled: gains.reflections != 0.0,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureQualitySettings {
    pub direct_occlusion: String,
    pub max_occlusion_samples: i32,
    pub reflection_effect: String,
    pub reflection_rays: i32,
    pub reflection_bounces: i32,
    pub reflection_duration_s: f32,
    pub reflection_order: i32,
    pub pathing_order: i32,
    pub validate_paths: bool,
    pub find_alternate_paths: bool,
    pub direct_simulation_hz: u32,
    pub pathing_simulation_hz: u32,
    pub reflections_simulation_hz: u32,
    pub reflection_max_displacement_m: f32,
    pub reflection_max_hz: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldPackageProvenance {
    pub path: String,
    pub package_manifest_sha256: Option<String>,
    pub mesh_content_sha256: String,
    pub materials_content_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BakeProvenance {
    pub path: String,
    pub identifier: Option<String>,
    pub bake_manifest_sha256: Option<String>,
    pub probe_batch_content_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureDraft {
    pub started_utc: String,
    pub fixture_id: String,
    pub fixture_path: String,
    pub fixture_content_sha256: String,
    pub engine_commit: Option<String>,
    pub engine_dirty: Option<bool>,
    pub world_package: WorldPackageProvenance,
    pub bake: BakeProvenance,
    pub sources: Vec<CaptureSourceState>,
    pub stages: CaptureStageState,
    pub quality: CaptureQualitySettings,
    pub listen_gain_db: f32,
    pub engine_config: CaptureEngineConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureEngineConfig {
    pub sample_rate_hz: u32,
    pub block_size_frames: u32,
    pub speed_of_sound_mps: f32,
    pub max_active_sources: u8,
}

impl CaptureEngineConfig {
    pub fn to_engine_config(self) -> EngineConfig {
        EngineConfig {
            sample_rate_hz: self.sample_rate_hz,
            block_size_frames: self.block_size_frames,
            speed_of_sound_mps: self.speed_of_sound_mps,
            max_active_sources: self.max_active_sources,
            ..EngineConfig::default()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureEndStats {
    pub callback_count: u64,
    pub window_p99_ms: f64,
    pub window_p99_9_ms: f64,
    pub run_p99_ms: f64,
    pub run_p99_9_ms: f64,
    pub deadline_misses: u64,
    pub processing_errors: u64,
    pub stream_errors: u64,
    pub snapshot_stale: u64,
    pub graph_deadline_miss: u64,
    pub backend_render_error: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaptureCompletion {
    pub bundle: PathBuf,
    pub result: Result<(), String>,
}

#[derive(Clone, Copy)]
struct CaptureAudioBlock {
    frames: u16,
    interleaved: [f32; CAPTURE_BLOCK_SAMPLES],
}

impl Default for CaptureAudioBlock {
    fn default() -> Self {
        Self {
            frames: 0,
            interleaved: [0.0; CAPTURE_BLOCK_SAMPLES],
        }
    }
}

struct SpscAudioRing {
    slots: Box<[UnsafeCell<CaptureAudioBlock>]>,
    read: AtomicUsize,
    write: AtomicUsize,
}

// SAFETY: there is one producer (the audio callback) and one consumer (the
// capture writer thread). The producer only writes the slot at `write`; the
// consumer only reads slots strictly before the acquire-loaded `write`.
unsafe impl Sync for SpscAudioRing {}

impl SpscAudioRing {
    fn new(capacity: usize) -> Self {
        let slots = (0..capacity)
            .map(|_| UnsafeCell::new(CaptureAudioBlock::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
        }
    }

    fn push(&self, block: CaptureAudioBlock) -> bool {
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        if write.wrapping_sub(read) >= self.slots.len() {
            return false;
        }
        let index = write % self.slots.len();
        // SAFETY: SPSC ownership and the capacity check make this slot
        // producer-exclusive until the release store publishes it.
        unsafe {
            *self.slots[index].get() = block;
        }
        self.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }

    fn pop(&self) -> Option<CaptureAudioBlock> {
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        if read == write {
            return None;
        }
        let index = read % self.slots.len();
        // SAFETY: the acquire load observed the producer's release store, and
        // this consumer alone advances `read`.
        let block = unsafe { *self.slots[index].get() };
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(block)
    }

    fn is_empty(&self) -> bool {
        self.read.load(Ordering::Acquire) == self.write.load(Ordering::Acquire)
    }
}

struct CaptureShared {
    ring: SpscAudioRing,
    requested: AtomicBool,
    producer_active: AtomicBool,
    captured_frames: AtomicU64,
    dropped_blocks: AtomicU64,
    auto_stopped: AtomicBool,
}

/// Audio-thread endpoint. Its block method performs only bounded copies and
/// atomic operations into preallocated storage.
pub struct CaptureTap {
    shared: Arc<CaptureShared>,
}

impl CaptureTap {
    pub fn capture_block(&self, left: &[f32], right: &[f32]) {
        if !self.shared.requested.load(Ordering::Acquire) {
            self.shared.producer_active.store(false, Ordering::Release);
            return;
        }
        self.shared.producer_active.store(true, Ordering::Release);

        let captured = self.shared.captured_frames.load(Ordering::Relaxed);
        let maximum = MAX_CAPTURE_SECONDS * 48_000;
        let remaining = maximum.saturating_sub(captured);
        let frames = left.len().min(right.len()).min(remaining as usize);
        if frames == 0 {
            self.shared.requested.store(false, Ordering::Release);
            self.shared.auto_stopped.store(true, Ordering::Release);
            self.shared.producer_active.store(false, Ordering::Release);
            return;
        }

        let mut block = CaptureAudioBlock {
            frames: frames as u16,
            ..CaptureAudioBlock::default()
        };
        for frame in 0..frames {
            block.interleaved[frame * 2] = left[frame];
            block.interleaved[frame * 2 + 1] = right[frame];
        }
        if !self.shared.ring.push(block) {
            self.shared.dropped_blocks.fetch_add(1, Ordering::Relaxed);
        }
        let total = self
            .shared
            .captured_frames
            .fetch_add(frames as u64, Ordering::Relaxed)
            .saturating_add(frames as u64);
        if total >= maximum {
            self.shared.requested.store(false, Ordering::Release);
            self.shared.auto_stopped.store(true, Ordering::Release);
        }
        self.shared.producer_active.store(
            self.shared.requested.load(Ordering::Acquire),
            Ordering::Release,
        );
    }
}

enum WriterCommand {
    Start {
        bundle: PathBuf,
        draft: CaptureDraft,
        wav: StreamingWavWriter,
    },
    Finish(CaptureEndStats),
    Shutdown,
}

struct WriterSession {
    bundle: PathBuf,
    draft: CaptureDraft,
    wav: StreamingWavWriter,
    write_error: Option<String>,
}

pub struct CaptureController {
    root: PathBuf,
    shared: Arc<CaptureShared>,
    commands: Sender<WriterCommand>,
    completions: Receiver<CaptureCompletion>,
    writer: Option<JoinHandle<()>>,
    busy: bool,
}

impl CaptureController {
    pub fn new(root: PathBuf) -> (Self, CaptureTap) {
        let shared = Arc::new(CaptureShared {
            ring: SpscAudioRing::new(CAPTURE_RING_BLOCKS),
            requested: AtomicBool::new(false),
            producer_active: AtomicBool::new(false),
            captured_frames: AtomicU64::new(0),
            dropped_blocks: AtomicU64::new(0),
            auto_stopped: AtomicBool::new(false),
        });
        let (command_tx, command_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let writer_shared = Arc::clone(&shared);
        let writer = thread::Builder::new()
            .name("fightbox-capture-writer".into())
            .spawn(move || writer_thread(writer_shared, command_rx, completion_tx))
            .expect("capture writer thread must start");
        (
            Self {
                root,
                shared: Arc::clone(&shared),
                commands: command_tx,
                completions: completion_rx,
                writer: Some(writer),
                busy: false,
            },
            CaptureTap { shared },
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn start(&mut self, draft: CaptureDraft) -> Result<PathBuf, String> {
        if self.busy || !self.shared.ring.is_empty() {
            return Err("capture writer is still finishing the preceding bundle".into());
        }
        fs::create_dir_all(&self.root)
            .map_err(|error| format!("cannot create capture root: {error}"))?;
        let bundle = unique_bundle_path(&self.root, &draft.started_utc);
        fs::create_dir(&bundle)
            .map_err(|error| format!("cannot create capture bundle: {error}"))?;
        let wav = StreamingWavWriter::create(&bundle.join("capture.wav"), 48_000, 2)
            .map_err(|error| format!("cannot create capture WAV: {error}"))?;

        self.shared.captured_frames.store(0, Ordering::Release);
        self.shared.dropped_blocks.store(0, Ordering::Release);
        self.shared.auto_stopped.store(false, Ordering::Release);
        self.commands
            .send(WriterCommand::Start {
                bundle: bundle.clone(),
                draft,
                wav,
            })
            .map_err(|_| "capture writer thread stopped".to_owned())?;
        self.busy = true;
        self.shared.requested.store(true, Ordering::Release);
        Ok(bundle)
    }

    pub fn request_stop(&self) {
        self.shared.requested.store(false, Ordering::Release);
    }

    pub fn is_requested(&self) -> bool {
        self.shared.requested.load(Ordering::Acquire)
    }

    pub fn ready_to_finish(&self) -> bool {
        !self.is_requested()
            && !self.shared.producer_active.load(Ordering::Acquire)
            && self.shared.ring.is_empty()
    }

    pub fn finish(&self, stats: CaptureEndStats) -> Result<(), String> {
        self.commands
            .send(WriterCommand::Finish(stats))
            .map_err(|_| "capture writer thread stopped".to_owned())
    }

    pub fn elapsed_seconds(&self) -> f64 {
        self.shared.captured_frames.load(Ordering::Acquire) as f64 / 48_000.0
    }

    pub fn was_auto_stopped(&self) -> bool {
        self.shared.auto_stopped.load(Ordering::Acquire)
    }

    pub fn poll_completion(&mut self) -> Option<CaptureCompletion> {
        match self.completions.try_recv() {
            Ok(completion) => {
                self.busy = false;
                Some(completion)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for CaptureController {
    fn drop(&mut self) {
        self.shared.requested.store(false, Ordering::Release);
        let _ = self.commands.send(WriterCommand::Shutdown);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn writer_thread(
    shared: Arc<CaptureShared>,
    commands: Receiver<WriterCommand>,
    completions: Sender<CaptureCompletion>,
) {
    let mut session: Option<WriterSession> = None;
    loop {
        match commands.try_recv() {
            Ok(WriterCommand::Start { bundle, draft, wav }) => {
                session = Some(WriterSession {
                    bundle,
                    draft,
                    wav,
                    write_error: None,
                });
            }
            Ok(WriterCommand::Finish(stats)) => {
                if let Some(mut active) = session.take() {
                    drain_ring(&shared, &mut active);
                    let dropped_blocks = shared.dropped_blocks.load(Ordering::Acquire);
                    let result = active
                        .write_error
                        .take()
                        .map_or_else(
                            || active.wav.finish().map_err(|error| error.to_string()),
                            Err,
                        )
                        .and_then(|frames| {
                            let manifest = build_capture_manifest(
                                &active.draft,
                                &stats,
                                frames,
                                dropped_blocks,
                            );
                            let bytes = serde_json::to_vec_pretty(&manifest)
                                .map_err(|error| error.to_string())?;
                            fs::write(active.bundle.join("manifest.json"), bytes)
                                .map_err(|error| error.to_string())
                        });
                    let _ = completions.send(CaptureCompletion {
                        bundle: active.bundle,
                        result,
                    });
                }
            }
            Ok(WriterCommand::Shutdown) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }
        if let Some(active) = &mut session {
            drain_ring(&shared, active);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn drain_ring(shared: &CaptureShared, session: &mut WriterSession) {
    while let Some(block) = shared.ring.pop() {
        if session.write_error.is_none()
            && let Err(error) = session.wav.append(&block)
        {
            session.write_error = Some(error.to_string());
        }
    }
}

struct StreamingWavWriter {
    file: File,
    sample_rate_hz: u32,
    channels: u16,
    sample_count: u64,
}

impl StreamingWavWriter {
    fn create(path: &Path, sample_rate_hz: u32, channels: u16) -> std::io::Result<Self> {
        let mut file = File::create(path)?;
        file.write_all(&wav_header(sample_rate_hz, channels, 0))?;
        Ok(Self {
            file,
            sample_rate_hz,
            channels,
            sample_count: 0,
        })
    }

    fn append(&mut self, block: &CaptureAudioBlock) -> std::io::Result<()> {
        let samples = usize::from(block.frames) * usize::from(self.channels);
        let mut bytes = [0_u8; CAPTURE_BLOCK_SAMPLES * 4];
        for (index, sample) in block.interleaved[..samples].iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&sample.to_le_bytes());
        }
        self.file.write_all(&bytes[..samples * 4])?;
        self.sample_count = self.sample_count.saturating_add(samples as u64);
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<u64> {
        let data_bytes = self
            .sample_count
            .checked_mul(4)
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| std::io::Error::other("capture WAV exceeds RIFF size"))?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file
            .write_all(&wav_header(self.sample_rate_hz, self.channels, data_bytes))?;
        self.file.flush()?;
        Ok(self.sample_count / u64::from(self.channels))
    }
}

fn wav_header(sample_rate_hz: u32, channels: u16, data_bytes: u32) -> [u8; 44] {
    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&data_bytes.saturating_add(36).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&3_u16.to_le_bytes());
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate_hz.to_le_bytes());
    let byte_rate = sample_rate_hz * u32::from(channels) * 4;
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&(channels * 4).to_le_bytes());
    header[34..36].copy_from_slice(&32_u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    header
}

fn build_capture_manifest(
    draft: &CaptureDraft,
    stats: &CaptureEndStats,
    frames: u64,
    dropped_blocks: u64,
) -> Value {
    let deadline_faults = stats
        .deadline_misses
        .saturating_add(stats.graph_deadline_miss);
    let base = CaptureRunManifest {
        fixture_id: FixtureId::new(&draft.fixture_id),
        kernel: KernelProvenance {
            name: "Steam Audio".into(),
            version: fightbox_steam_audio::STEAM_AUDIO_VERSION.into(),
            upstream_commit: fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT.into(),
            binary_checksum_sha256: None,
        },
        config: CaptureConfig {
            engine: draft.engine_config.to_engine_config(),
            build_profile: if cfg!(debug_assertions) {
                "debug".into()
            } else {
                "release".into()
            },
            requested_quality: "fixture-defined live convolution".into(),
            delivered_quality: Some("fixture-defined live convolution".into()),
        },
        stems: Vec::<StemRecord>::new(),
        state: RunState::Completed,
        claims: vec![ExplicitClaim {
            statement: "capture.wav is the live summed binaural monitor output".into(),
        }],
        non_claims: vec![ExplicitNonClaim {
            statement: "no per-stage stems or in-app playback are included".into(),
        }],
        world: Some(WorldProvenance {
            world_content_sha256: draft.world_package.package_manifest_sha256.clone(),
            bake_content_sha256: draft.bake.bake_manifest_sha256.clone(),
            probe_batch_content_sha256: Some(draft.bake.probe_batch_content_sha256.clone()),
        }),
        source_calibrations: vec![],
        pathing_toggle: None,
        metrics: vec![],
        provenance: RunProvenance {
            engine_commit: draft.engine_commit.clone(),
            platform: Some(std::env::consts::OS.into()),
            cpu_class: Some(std::env::consts::ARCH.into()),
            hrtf_identity: Some("Steam Audio default HRTF".into()),
            fixture_content_sha256: Some(draft.fixture_content_sha256.clone()),
            bake_duration_s: None,
            render_duration_s: Some(frames as f32 / draft.engine_config.sample_rate_hz as f32),
            simulation_cadence: SimulationCadence::default(),
            callback_timing: CallbackTiming {
                status: if deadline_faults == 0 {
                    CallbackStatus::Met
                } else {
                    CallbackStatus::Faulted
                },
                deadline_fault_count: deadline_faults.min(u64::from(u32::MAX)) as u32,
                max_callback_overrun_s: None,
            },
            limiter_events: vec![],
            degradation_events: vec![],
        },
    };
    let mut value: Value =
        serde_json::from_str(&base.to_json()).expect("evidence manifest JSON is valid");
    let object = value
        .as_object_mut()
        .expect("evidence manifest root is an object");
    object.insert(
        "workbench_capture".into(),
        json!({
            "schema_version": CAPTURE_SCHEMA_VERSION,
            "started_utc": draft.started_utc,
            "capture_file": "capture.wav",
            "frames": frames,
            "duration_seconds": frames as f64 / draft.engine_config.sample_rate_hz as f64,
            "listen_gain_db": draft.listen_gain_db,
            "dropped_capture_blocks": dropped_blocks,
            "engine_dirty": draft.engine_dirty,
            "fixture_path": draft.fixture_path,
            "world_package": draft.world_package,
            "bake": draft.bake,
            "sources": draft.sources,
            "stages": draft.stages,
            "quality": draft.quality,
        }),
    );
    if let Some(Value::Object(provenance)) = object.get_mut("provenance") {
        provenance.insert(
            "callback_detail".into(),
            serde_json::to_value(stats).expect("callback detail is serializable"),
        );
    }
    value
}

fn unique_bundle_path(root: &Path, timestamp: &str) -> PathBuf {
    let candidate = root.join(timestamp);
    if !candidate.exists() {
        return candidate;
    }
    for suffix in 1..10_000 {
        let candidate = root.join(format!("{timestamp}-{suffix:02}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    root.join(format!("{timestamp}-overflow"))
}

pub fn default_capture_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("fightbox-runs")
        .join("workbench-captures"))
}

pub fn utc_timestamp_now() -> String {
    utc_timestamp(SystemTime::now())
}

fn utc_timestamp(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}.{:03}Z",
        duration.subsec_millis()
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaptureBrowserEntry {
    pub bundle: PathBuf,
    pub timestamp: String,
    pub duration_seconds: f64,
    pub fixture_id: String,
    pub run_p99_ms: f64,
    pub run_p99_9_ms: f64,
    pub deadline_misses: u64,
}

#[derive(Default)]
pub struct BrowserScan {
    pub entries: Vec<CaptureBrowserEntry>,
    pub warnings: Vec<String>,
}

pub fn scan_capture_bundles(root: &Path) -> Result<BrowserScan, String> {
    if !root.exists() {
        return Ok(BrowserScan::default());
    }
    let directories =
        fs::read_dir(root).map_err(|error| format!("cannot read capture directory: {error}"))?;
    let mut scan = BrowserScan::default();
    for directory in directories {
        let directory = match directory {
            Ok(value) => value,
            Err(error) => {
                scan.warnings.push(error.to_string());
                continue;
            }
        };
        let bundle = directory.path();
        if !bundle.is_dir() {
            continue;
        }
        match parse_browser_entry(&bundle) {
            Ok(entry) => scan.entries.push(entry),
            Err(error) => scan.warnings.push(format!("{}: {error}", bundle.display())),
        }
    }
    scan.entries
        .sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    Ok(scan)
}

fn parse_browser_entry(bundle: &Path) -> Result<CaptureBrowserEntry, String> {
    let bytes = fs::read(bundle.join("manifest.json"))
        .map_err(|error| format!("cannot read manifest: {error}"))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid manifest: {error}"))?;
    let get_str = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("missing {pointer}"))
    };
    let get_f64 = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("missing {pointer}"))
    };
    Ok(CaptureBrowserEntry {
        bundle: bundle.to_owned(),
        timestamp: get_str("/workbench_capture/started_utc")?,
        duration_seconds: get_f64("/workbench_capture/duration_seconds")?,
        fixture_id: get_str("/fixture_id")?,
        run_p99_ms: get_f64("/provenance/callback_detail/run_p99_ms")?,
        run_p99_9_ms: get_f64("/provenance/callback_detail/run_p99_9_ms")?,
        deadline_misses: value
            .pointer("/provenance/callback_detail/deadline_misses")
            .and_then(Value::as_u64)
            .ok_or("missing deadline_misses")?,
    })
}

pub fn reveal_in_finder(path: &Path) -> Result<(), String> {
    Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot reveal capture: {error}"))
}

pub fn git_identity(repository: &Path) -> (Option<String>, Option<bool>) {
    let commit = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let dirty = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty());
    (commit, dirty)
}

pub fn sha256_file(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|bytes| fightbox_evidence::sha256_hex(&bytes))
}

pub fn json_string_field(path: &Path, pointer: &str) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.pointer(pointer)?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> CaptureDraft {
        CaptureDraft {
            started_utc: "2026-07-30T03-22-00.000Z".into(),
            fixture_id: "fixture-a".into(),
            fixture_path: "/fixtures/a.json".into(),
            fixture_content_sha256: "f".repeat(64),
            engine_commit: Some("abc".into()),
            engine_dirty: Some(true),
            world_package: WorldPackageProvenance {
                path: "/world".into(),
                package_manifest_sha256: Some("a".repeat(64)),
                mesh_content_sha256: "b".repeat(64),
                materials_content_sha256: "c".repeat(64),
            },
            bake: BakeProvenance {
                path: "/bake".into(),
                identifier: Some("bake-a".into()),
                bake_manifest_sha256: Some("d".repeat(64)),
                probe_batch_content_sha256: "e".repeat(64),
            },
            sources: vec![CaptureSourceState {
                id: "source".into(),
                asset_id: "asset".into(),
                enabled: true,
                muted: false,
                soloed: true,
            }],
            stages: CaptureStageState::from(StageMix::ALL_ENABLED),
            quality: CaptureQualitySettings {
                direct_occlusion: "raycast".into(),
                max_occlusion_samples: 64,
                reflection_effect: "convolution".into(),
                reflection_rays: 4_096,
                reflection_bounces: 3,
                reflection_duration_s: 1.5,
                reflection_order: 1,
                pathing_order: 2,
                validate_paths: true,
                find_alternate_paths: true,
                direct_simulation_hz: 60,
                pathing_simulation_hz: 15,
                reflections_simulation_hz: 5,
                reflection_max_displacement_m: 1.0,
                reflection_max_hz: 25,
            },
            listen_gain_db: 50.0,
            engine_config: CaptureEngineConfig {
                sample_rate_hz: 48_000,
                block_size_frames: 128,
                speed_of_sound_mps: 343.0,
                max_active_sources: 1,
            },
        }
    }

    #[test]
    fn capture_manifest_serialization_round_trips() {
        let manifest = build_capture_manifest(&draft(), &CaptureEndStats::default(), 48_000, 0);
        let serialized = serde_json::to_vec_pretty(&manifest).unwrap();
        let decoded: Value = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(
            decoded.pointer("/kernel/version").and_then(Value::as_str),
            Some("4.8.1")
        );
        assert_eq!(
            decoded
                .pointer("/workbench_capture/sources/0/soloed")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn browser_directory_parsing_lists_valid_bundles_and_skips_invalid_ones() {
        let unique = format!(
            "fightbox-capture-browser-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let valid = root.join("valid");
        let invalid = root.join("invalid");
        fs::create_dir_all(&valid).unwrap();
        fs::create_dir_all(&invalid).unwrap();
        let manifest = build_capture_manifest(&draft(), &CaptureEndStats::default(), 48_000, 0);
        fs::write(
            valid.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(invalid.join("manifest.json"), b"not json").unwrap();

        let scan = scan_capture_bundles(&root).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].fixture_id, "fixture-a");
        assert_eq!(scan.entries[0].duration_seconds, 1.0);
        assert_eq!(scan.warnings.len(), 1);
    }

    #[test]
    fn utc_timestamp_formats_the_unix_epoch() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "1970-01-01T00-00-00.000Z");
    }
}
