//! Phase B B2 commands: deterministic S6a evidence and four-source soaks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fightbox_api::{
    EngineConfig, EnuVector3, ExtentDescriptor, ListenerState, Pose, ReferenceLevel,
    SceneCalibration, SourceId, SourceProfile,
};
use fightbox_evidence::sha256_hex;
use fightbox_runtime::backend::{SimulationRunner, SimulationUpdate, SourceMotion};
use fightbox_runtime::{
    FaultCounters, MAX_ACTIVE_SOURCES, OfflineDriver, ProcessBlock, PropagationSnapshot,
    RuntimeGraph, SnapshotPublication, SourceBlock, SourcePropagation, TimingHistory,
    TimingPercentiles,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, BakedProbeBatch, DirectOcclusionMode,
    EnuVector3 as SteamEnuVector3, MultiSourceDescriptor, PathBakeConfig, ProbeVolume,
    ReflectionEffectConfig, S3BakeRequest, S3SimulationConfig, SceneMesh, bake_s3,
    build_multi_source_session,
};
use serde::{Deserialize, Serialize};

use crate::asset::AssetDescriptor;
use crate::atomicio::{self, AtomicDir, validate_output_path, write_json_atomic};
use crate::error::{CliError, Result};

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_SIZE: usize = 128;
const P99_GATE_MS: f64 = 1.33;
const P99_9_GATE_MS: f64 = 2.13;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    fixture_id: String,
    gate: String,
    coordinate_frame: serde_json::Value,
    kernel: serde_json::Value,
    sources: Vec<FixtureSource>,
    listener: FixtureListener,
    geometry: FixtureGeometry,
    simulation: FixtureSimulation,
    expected: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureSource {
    id: String,
    asset_id: String,
    reference_level: FixtureReferenceLevel,
    position_m: Option<[f64; 3]>,
    trajectory: Option<Trajectory>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureReferenceLevel {
    mode: ReferenceMode,
    db_spl: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum ReferenceMode {
    SplAtOneMeter,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trajectory {
    waypoints_m: Vec<[f64; 3]>,
    speed_mps: f64,
    max_speed_mps: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureListener {
    position_m: Option<[f64; 3]>,
    trajectory: Option<Trajectory>,
    forward_enu: [f64; 3],
    up_enu: [f64; 3],
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureGeometry {
    vertices_m: Vec<[f64; 3]>,
    triangles: Vec<FixtureTriangle>,
    materials: BTreeMap<String, FixtureMaterial>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureTriangle {
    indices: [usize; 3],
    material: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureMaterial {
    absorption: [f64; 3],
    scattering: f64,
    transmission: [f64; 3],
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureSimulation {
    direct: FixtureDirect,
    reflections: FixtureReflections,
    pathing: FixturePathing,
    probe_volume: FixtureProbeVolume,
    probe_generation: FixtureProbeGeneration,
    path_bake: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureDirect {
    distance_attenuation: bool,
    occlusion: bool,
    occlusion_samples: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureReflections {
    enabled: bool,
    rays: Option<u32>,
    bounces: Option<u32>,
    duration_s: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixturePathing {
    enabled: bool,
    order: Option<u32>,
    validation: Option<bool>,
    alternate_paths: Option<bool>,
    runtime_order: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureProbeVolume {
    #[serde(rename = "type")]
    kind: ProbeVolumeKind,
    min_m: [f64; 3],
    max_m: [f64; 3],
    spacing_m: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProbeVolumeKind {
    Box,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureProbeGeneration {
    #[serde(rename = "type")]
    kind: ProbeGenerationKind,
    height_m: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProbeGenerationKind {
    UniformFloor,
}

#[derive(Clone)]
struct PreparedSource {
    id: String,
    asset_id: String,
    descriptor_hash: String,
    signal: Vec<f32>,
    profile: SourceProfile,
}

struct PreparedFixture {
    fixture: Fixture,
    fixture_hash: String,
    sources: Vec<PreparedSource>,
    mesh: SceneMesh,
    probes: ProbeVolume,
    simulation: S3SimulationConfig,
    timeline_frames: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixtureUse {
    PhaseBS6a,
    City,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct FaultReport {
    snapshot_stale: u64,
    deadline_miss: u64,
    backend_render_error: u64,
}

impl From<FaultCounters> for FaultReport {
    fn from(value: FaultCounters) -> Self {
        Self {
            snapshot_stale: value.snapshot_stale,
            deadline_miss: value.deadline_miss,
            backend_render_error: value.backend_render_error,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct Percentiles {
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    p99_9_ms: f64,
}

impl From<TimingPercentiles> for Percentiles {
    fn from(value: TimingPercentiles) -> Self {
        Self {
            p50_ms: value.p50_ms,
            p95_ms: value.p95_ms,
            p99_ms: value.p99_ms,
            p99_9_ms: value.p99_9_ms,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
struct PassReport {
    runs: u64,
    failures: u64,
    timings: Percentiles,
}

#[derive(Default)]
struct PassRecorder {
    runs: u64,
    failures: u64,
    timings: TimingHistory,
}

impl PassRecorder {
    fn record(
        &mut self,
        result: std::result::Result<(), fightbox_runtime::backend::SimulationError>,
        duration_ns: u64,
    ) {
        self.runs += 1;
        self.timings.record(duration_ns);
        self.failures += u64::from(result.is_err());
    }

    fn report(&self) -> PassReport {
        PassReport {
            runs: self.runs,
            failures: self.failures,
            timings: TimingPercentiles::from_history(&self.timings).into(),
        }
    }
}

#[derive(Default)]
struct SimulationTelemetry {
    direct: PassRecorder,
    pathing: PassRecorder,
    reflections: PassRecorder,
}

#[derive(Clone, Debug, Serialize)]
struct SimulationReport {
    direct: PassReport,
    pathing: PassReport,
    reflections: PassReport,
}

impl SimulationTelemetry {
    fn report(&self) -> SimulationReport {
        SimulationReport {
            direct: self.direct.report(),
            pathing: self.pathing.report(),
            reflections: self.reflections.report(),
        }
    }
}

struct Rendered {
    pcm: Vec<f32>,
    timings: TimingHistory,
    faults: FaultCounters,
    simulation: SimulationTelemetry,
}

#[derive(Serialize)]
struct IdentityReport {
    fixture_id: String,
    fixture_sha256: String,
    assets: Vec<AssetIdentity>,
    probe_batch_sha256: String,
}

#[derive(Serialize)]
struct AssetIdentity {
    source_index: usize,
    source_id: String,
    asset_id: String,
    descriptor_sha256: String,
}

#[derive(Serialize)]
struct WavReport {
    role: &'static str,
    source_index: Option<usize>,
    source_id: Option<String>,
    file: String,
    sha256: String,
    frame_count: usize,
}

#[derive(Serialize)]
struct ListeningReport {
    target_peak_dbfs: f64,
    source_peak_dbfs: Option<f64>,
    applied_gain_db: f64,
    applied_gain_linear: f32,
    wavs: Vec<ListeningWavReport>,
}

#[derive(Serialize)]
struct ListeningWavReport {
    role: &'static str,
    source_index: Option<usize>,
    source_id: Option<String>,
    file: String,
    sha256: String,
    frame_count: usize,
}

#[derive(Serialize)]
struct IsolationPair {
    muted_source_index: usize,
    compared_source_index: usize,
    baseline_sha256: String,
    muted_run_sha256: String,
    passed: bool,
}

#[derive(Serialize)]
struct S6aReport {
    schema_version: &'static str,
    reflection_effect: &'static str,
    sample_rate_hz: u32,
    block_size_frames: usize,
    timeline_frames: usize,
    timeline_seconds: f64,
    identity: IdentityReport,
    wavs: Vec<WavReport>,
    listening: ListeningReport,
    block_timings: Percentiles,
    faults: FaultReport,
    simulation: SimulationReport,
    isolation_check_requested: bool,
    isolation_passed: Option<bool>,
    isolation_pairs: Vec<IsolationPair>,
}

#[derive(Serialize)]
struct SoakGates {
    run_p99_limit_ms: f64,
    run_p99_actual_ms: f64,
    run_p99_passed: bool,
    run_p99_9_limit_ms: f64,
    run_p99_9_actual_ms: f64,
    run_p99_9_passed: bool,
    passed: bool,
}

#[derive(Serialize)]
struct SoakReport {
    schema_version: &'static str,
    mode: &'static str,
    reflection_effect: &'static str,
    requested_minutes: u64,
    rendered_blocks: u64,
    window_callback_timings: Percentiles,
    run_callback_timings: Percentiles,
    deadline_misses: u64,
    faults: FaultReport,
    gates: SoakGates,
    identity: IdentityReport,
}

pub fn run_s6a(
    fixture_path: &Path,
    output: &Path,
    isolation_check: bool,
    reflection_effect: ReflectionEffectConfig,
) -> Result<()> {
    require_linked("phase-b s6a")?;
    let output = validate_output_path(output)?;
    let prepared = prepare_fixture(fixture_path, reflection_effect, FixtureUse::PhaseBS6a)?;
    let baked = bake_fixture(&prepared)?;
    write_s6a_render(
        &prepared,
        &baked,
        &output,
        isolation_check,
        reflection_effect,
        None,
    )
}

/// Reuse the S6a multi-source graph and rendered-time stepping with a
/// package-supplied city mesh and a separately persisted probe bake.
pub(crate) fn run_city_render(
    fixture_path: &Path,
    output: &Path,
    mesh: SceneMesh,
    baked: &BakedProbeBatch,
    city_identity: CityRenderIdentity,
) -> Result<()> {
    require_linked("city render")?;
    let output = validate_output_path(output)?;
    let mut prepared = prepare_fixture(
        fixture_path,
        ReflectionEffectConfig::PARAMETRIC,
        FixtureUse::City,
    )?;
    prepared.mesh = mesh;
    write_s6a_render(
        &prepared,
        baked,
        &output,
        false,
        ReflectionEffectConfig::PARAMETRIC,
        Some(&city_identity),
    )
}

pub(crate) struct CityRenderIdentity {
    pub mesh_content_sha256: String,
    pub materials_content_sha256: String,
    pub probe_batch_sha256: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CityPerceptLevels {
    pub shadow_rms_dbfs: f64,
    pub los_rms_dbfs: f64,
}

/// Measures a bounded city occlusion percept at the two listener positions
/// declared by a dedicated fixture's listener trajectory. Each position is
/// rendered statically for one second; the second half is measured so startup
/// state is excluded from the RMS comparison.
pub(crate) fn measure_city_occlusion_percept(
    fixture_path: &Path,
    mesh: SceneMesh,
    baked: &BakedProbeBatch,
) -> Result<CityPerceptLevels> {
    require_linked("city metamorphic")?;
    let mut prepared = prepare_fixture(
        fixture_path,
        ReflectionEffectConfig::PARAMETRIC,
        FixtureUse::City,
    )?;
    if prepared.fixture.sources.len() != 1 || prepared.fixture.sources[0].position_m.is_none() {
        return Err(CliError::new(
            "city metamorphic fixture requires exactly one static source",
        ));
    }
    let positions = prepared
        .fixture
        .listener
        .trajectory
        .as_ref()
        .filter(|trajectory| trajectory.waypoints_m.len() == 2)
        .map(|trajectory| [trajectory.waypoints_m[0], trajectory.waypoints_m[1]])
        .ok_or_else(|| {
            CliError::new(
                "city metamorphic fixture listener trajectory must contain shadow and LOS positions",
            )
        })?;
    prepared.mesh = mesh;
    prepared.timeline_frames = SAMPLE_RATE as usize;

    prepared.fixture.listener.position_m = Some(positions[0]);
    prepared.fixture.listener.trajectory = None;
    let shadow = render(&prepared, baked, &[true])?;
    prepared.fixture.listener.position_m = Some(positions[1]);
    let los = render(&prepared, baked, &[true])?;
    Ok(CityPerceptLevels {
        shadow_rms_dbfs: settled_stereo_rms_dbfs(&shadow.pcm)?,
        los_rms_dbfs: settled_stereo_rms_dbfs(&los.pcm)?,
    })
}

fn settled_stereo_rms_dbfs(pcm: &[f32]) -> Result<f64> {
    let settled = &pcm[pcm.len() / 2..];
    let mean_square = settled
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / settled.len() as f64;
    if mean_square <= 0.0 || !mean_square.is_finite() {
        return Err(CliError::new(
            "city metamorphic percept rendered silence or non-finite PCM",
        ));
    }
    Ok(20.0 * mean_square.sqrt().log10())
}

fn write_s6a_render(
    prepared: &PreparedFixture,
    baked: &BakedProbeBatch,
    output: &Path,
    isolation_check: bool,
    reflection_effect: ReflectionEffectConfig,
    city_identity: Option<&CityRenderIdentity>,
) -> Result<()> {
    let mix = render(&prepared, &baked, &vec![true; prepared.sources.len()])?;
    let mut stems = Vec::new();
    for index in 0..prepared.sources.len() {
        let mut enabled = vec![false; prepared.sources.len()];
        enabled[index] = true;
        stems.push(render(&prepared, &baked, &enabled)?);
    }

    let dir = AtomicDir::create(output.to_path_buf())?;
    let temp = dir.temp_path();
    let mut wavs = Vec::new();
    let mix_written = atomicio::write_stereo_wav(temp, "mix.wav", SAMPLE_RATE as i32, &mix.pcm)?;
    wavs.push(WavReport {
        role: "mix",
        source_index: None,
        source_id: None,
        file: "mix.wav".into(),
        sha256: mix_written.content_sha256,
        frame_count: mix_written.frame_count,
    });
    let mut baseline_hashes = Vec::new();
    for (index, stem) in stems.iter().enumerate() {
        let file = format!("stem-{}-{}.wav", index + 1, prepared.sources[index].id);
        let written = atomicio::write_stereo_wav(temp, &file, SAMPLE_RATE as i32, &stem.pcm)?;
        baseline_hashes.push(written.content_sha256.clone());
        wavs.push(WavReport {
            role: "stem",
            source_index: Some(index),
            source_id: Some(prepared.sources[index].id.clone()),
            file,
            sha256: written.content_sha256,
            frame_count: written.frame_count,
        });
    }
    let listening = write_listening_copies(temp, &mix, &stems, prepared)?;

    let mut pairs = Vec::new();
    if isolation_check {
        for muted in 0..prepared.sources.len() {
            for compared in 0..prepared.sources.len() {
                if muted == compared {
                    continue;
                }
                let mut enabled = vec![false; prepared.sources.len()];
                enabled[compared] = true;
                let rerendered = render(&prepared, &baked, &enabled)?;
                let bytes = fightbox_evidence::write_wav(
                    fightbox_evidence::WavSpec {
                        sample_rate_hz: SAMPLE_RATE,
                        channels: 2,
                    },
                    &rerendered.pcm,
                )
                .map_err(|error| {
                    CliError::new(format!("cannot encode isolation WAV: {error:?}"))
                })?;
                let rerendered_hash = sha256_hex(&bytes);
                pairs.push(IsolationPair {
                    muted_source_index: muted,
                    compared_source_index: compared,
                    passed: rerendered_hash == baseline_hashes[compared],
                    baseline_sha256: baseline_hashes[compared].clone(),
                    muted_run_sha256: rerendered_hash,
                });
            }
        }
    }
    let isolation_passed = isolation_check.then(|| pairs.iter().all(|pair| pair.passed));
    if isolation_passed == Some(false) {
        return Err(CliError::new(
            "source-isolation check failed; output was not committed",
        ));
    }
    let report = S6aReport {
        schema_version: "fightbox.phase-b.s6a-report.v1",
        reflection_effect: reflection_effect_name(reflection_effect),
        sample_rate_hz: SAMPLE_RATE,
        block_size_frames: BLOCK_SIZE,
        timeline_frames: prepared.timeline_frames,
        timeline_seconds: prepared.timeline_frames as f64 / f64::from(SAMPLE_RATE),
        identity: identity(&prepared, &baked),
        wavs,
        listening,
        block_timings: TimingPercentiles::from_history(&mix.timings).into(),
        faults: mix.faults.into(),
        simulation: mix.simulation.report(),
        isolation_check_requested: isolation_check,
        isolation_passed,
        isolation_pairs: pairs,
    };
    write_json_atomic(&temp.join("report.json"), &report)?;
    if let Some(identity) = city_identity {
        write_json_atomic(
            &temp.join("city-render-manifest.json"),
            &serde_json::json!({
                "schema_version": "fightbox.city-render.v1",
                "materials_content_sha256": identity.materials_content_sha256,
                "mesh_content_sha256": identity.mesh_content_sha256,
                "probe_batch_sha256": identity.probe_batch_sha256,
            }),
        )?;
    }
    dir.commit()?;
    eprintln!(
        "fightbox: S6a render written to {} ({} frames, isolation {})",
        output.display(),
        prepared.timeline_frames,
        if isolation_check {
            "passed"
        } else {
            "not requested"
        }
    );
    Ok(())
}

fn write_listening_copies(
    directory: &Path,
    mix: &Rendered,
    stems: &[Rendered],
    prepared: &PreparedFixture,
) -> Result<ListeningReport> {
    const TARGET_PEAK_DBFS: f64 = -3.0;
    let signals =
        std::iter::once(mix.pcm.as_slice()).chain(stems.iter().map(|stem| stem.pcm.as_slice()));
    let source_peak = signals
        .flat_map(|signal| signal.iter())
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max);
    let gain = listening_gain(source_peak, TARGET_PEAK_DBFS);
    let mut wavs = Vec::with_capacity(stems.len() + 1);

    let mix_pcm = apply_gain(&mix.pcm, gain);
    let mix_written =
        atomicio::write_stereo_wav(directory, "LISTEN-mix.wav", SAMPLE_RATE as i32, &mix_pcm)?;
    wavs.push(ListeningWavReport {
        role: "mix",
        source_index: None,
        source_id: None,
        file: "LISTEN-mix.wav".into(),
        sha256: mix_written.content_sha256,
        frame_count: mix_written.frame_count,
    });
    for (index, stem) in stems.iter().enumerate() {
        let file = format!(
            "LISTEN-stem-{}-{}.wav",
            index + 1,
            prepared.sources[index].id
        );
        let pcm = apply_gain(&stem.pcm, gain);
        let written = atomicio::write_stereo_wav(directory, &file, SAMPLE_RATE as i32, &pcm)?;
        wavs.push(ListeningWavReport {
            role: "stem",
            source_index: Some(index),
            source_id: Some(prepared.sources[index].id.clone()),
            file,
            sha256: written.content_sha256,
            frame_count: written.frame_count,
        });
    }

    Ok(ListeningReport {
        target_peak_dbfs: TARGET_PEAK_DBFS,
        source_peak_dbfs: amplitude_dbfs(source_peak),
        applied_gain_db: if source_peak > 0.0 {
            20.0 * f64::from(gain).log10()
        } else {
            0.0
        },
        applied_gain_linear: gain,
        wavs,
    })
}

fn listening_gain(source_peak: f32, target_peak_dbfs: f64) -> f32 {
    if source_peak <= 0.0 {
        return 1.0;
    }
    (10.0_f64.powf(target_peak_dbfs / 20.0) / f64::from(source_peak)) as f32
}

fn apply_gain(samples: &[f32], gain: f32) -> Vec<f32> {
    samples.iter().map(|sample| sample * gain).collect()
}

fn amplitude_dbfs(amplitude: f32) -> Option<f64> {
    if amplitude > 0.0 {
        Some(20.0 * f64::from(amplitude).log10())
    } else {
        None
    }
}

pub fn run_soak(
    minutes: u64,
    output: &Path,
    live: bool,
    reflection_effect: ReflectionEffectConfig,
) -> Result<()> {
    if minutes == 0 {
        return Err(CliError::new("--minutes must be greater than zero"));
    }
    require_linked("phase-b soak")?;
    let output = validate_output_path(output)?;
    let fixture_path = repo_root().join("fixtures/s6a-four-sources/fixture.json");
    let prepared = prepare_fixture(&fixture_path, reflection_effect, FixtureUse::PhaseBS6a)?;
    let baked = bake_fixture(&prepared)?;
    let seconds = minutes
        .checked_mul(60)
        .ok_or_else(|| CliError::new("--minutes is too large"))?;
    let report = if live {
        live_soak(&prepared, &baked, seconds)?
    } else {
        offline_soak(&prepared, &baked, seconds)?
    };
    let window_timings: Percentiles = report.window_callback_timings.into();
    let run_timings: Percentiles = report.run_callback_timings.into();
    let run_p99_passed = run_timings.p99_ms <= P99_GATE_MS;
    let run_p99_9_passed = run_timings.p99_9_ms <= P99_9_GATE_MS;
    let wire = SoakReport {
        schema_version: "fightbox.phase-b.soak-report.v2",
        mode: if live { "live" } else { "offline" },
        reflection_effect: reflection_effect_name(reflection_effect),
        requested_minutes: minutes,
        rendered_blocks: report.rendered_blocks,
        window_callback_timings: window_timings,
        run_callback_timings: run_timings,
        deadline_misses: report.deadline_misses,
        faults: report.faults.into(),
        gates: SoakGates {
            run_p99_limit_ms: P99_GATE_MS,
            run_p99_actual_ms: run_timings.p99_ms,
            run_p99_passed,
            run_p99_9_limit_ms: P99_9_GATE_MS,
            run_p99_9_actual_ms: run_timings.p99_9_ms,
            run_p99_9_passed,
            passed: run_p99_passed && run_p99_9_passed,
        },
        identity: identity(&prepared, &baked),
    };
    let dir = AtomicDir::create(output.clone())?;
    write_json_atomic(&dir.temp_path().join("report.json"), &wire)?;
    dir.commit()?;
    eprintln!("fightbox: soak report written to {}", output.display());
    Ok(())
}

fn offline_soak(
    prepared: &PreparedFixture,
    baked: &BakedProbeBatch,
    seconds: u64,
) -> Result<fightbox_runtime::SoakReport> {
    let (mut runner, graph) = build_graph(prepared, baked)?;
    runner.update_inputs(&simulation_update(prepared, 0));
    initial_simulation(&mut runner)?;
    let buffers: Vec<Vec<f32>> = prepared
        .sources
        .iter()
        .map(|source| {
            (0..BLOCK_SIZE)
                .map(|frame| source.signal[frame % source.signal.len()])
                .collect()
        })
        .collect();
    let sources: Vec<SourceBlock<'_>> = buffers
        .iter()
        .enumerate()
        .map(|(source_index, decoded_mono)| SourceBlock {
            source_index,
            decoded_mono,
        })
        .collect();
    let mut graph = graph;
    fightbox_runtime::run_offline_soak(&mut graph, SAMPLE_RATE, seconds, &sources)
        .map_err(|error| CliError::new(format!("offline soak failed: {error:?}")))
}

#[cfg(feature = "live-output")]
fn live_soak(
    prepared: &PreparedFixture,
    baked: &BakedProbeBatch,
    seconds: u64,
) -> Result<fightbox_runtime::SoakReport> {
    use fightbox_runtime::live::{LiveInputProvider, LiveSourceBuffer};

    struct LoopingInput {
        signals: Vec<Vec<f32>>,
        offsets: Vec<usize>,
    }
    impl LiveInputProvider for LoopingInput {
        fn fill_block(&mut self, sources: &mut LiveSourceBuffer) {
            for index in 0..self.signals.len() {
                let Some(output) = sources.add_source(index) else {
                    return;
                };
                let mut offset = self.offsets[index];
                for sample in output {
                    *sample = self.signals[index][offset];
                    offset = (offset + 1) % self.signals[index].len();
                }
                self.offsets[index] = offset;
            }
        }
    }
    let (mut runner, graph) = build_graph(prepared, baked)?;
    runner.update_inputs(&simulation_update(prepared, 0));
    initial_simulation(&mut runner)?;
    let input = LoopingInput {
        signals: prepared
            .sources
            .iter()
            .map(|source| source.signal.clone())
            .collect(),
        offsets: vec![0; prepared.sources.len()],
    };
    fightbox_runtime::live::run_live_soak_with_input(
        graph,
        engine_config(prepared.sources.len()),
        Box::new(input),
        seconds,
    )
    .map_err(|error| CliError::new(format!("live soak failed: {error:?}")))
}

#[cfg(not(feature = "live-output"))]
fn live_soak(
    _prepared: &PreparedFixture,
    _baked: &BakedProbeBatch,
    _seconds: u64,
) -> Result<fightbox_runtime::SoakReport> {
    Err(CliError::new(
        "--live requires rebuilding with --features live-output,linked-sdk",
    ))
}

fn render(
    prepared: &PreparedFixture,
    baked: &BakedProbeBatch,
    enabled: &[bool],
) -> Result<Rendered> {
    let (mut runner, graph) = build_graph(prepared, baked)?;
    let mut driver = OfflineDriver::new(graph);
    let mut pcm = Vec::with_capacity(prepared.timeline_frames * 2);
    let mut timings = TimingHistory::default();
    let mut simulation = SimulationTelemetry::default();
    let mut source_buffers = vec![vec![0.0; BLOCK_SIZE]; prepared.sources.len()];
    let silence = [0.0_f32; BLOCK_SIZE];
    let mut next_ticks = [0_u64; 3];

    for block_index in 0..prepared.timeline_frames.div_ceil(BLOCK_SIZE) {
        let rendered_frames = block_index * BLOCK_SIZE;
        driver
            .processor_mut()
            .set_listener_state(listener_state_at(&prepared.fixture, rendered_frames as u64));
        step_simulation(
            &mut runner,
            prepared,
            rendered_frames as u64,
            &mut next_ticks,
            &mut simulation,
        )?;
        for (index, buffer) in source_buffers.iter_mut().enumerate() {
            for (frame, sample) in buffer.iter_mut().enumerate() {
                let timeline_frame = rendered_frames + frame;
                *sample = if enabled[index] && timeline_frame < prepared.timeline_frames {
                    prepared.sources[index].signal
                        [timeline_frame % prepared.sources[index].signal.len()]
                } else {
                    0.0
                };
            }
        }
        let sources: Vec<SourceBlock<'_>> = source_buffers
            .iter()
            .enumerate()
            .map(|(index, buffer)| SourceBlock {
                source_index: index,
                decoded_mono: if enabled[index] { buffer } else { &silence },
            })
            .collect();
        let mut left = [0.0; BLOCK_SIZE];
        let mut right = [0.0; BLOCK_SIZE];
        let started = Instant::now();
        driver
            .process_block(ProcessBlock {
                now_ns: rendered_frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE),
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            })
            .map_err(|error| CliError::new(format!("S6a render failed: {error:?}")))?;
        timings.record(elapsed_ns(started));
        let valid = (prepared.timeline_frames - rendered_frames).min(BLOCK_SIZE);
        for frame in 0..valid {
            pcm.extend_from_slice(&[left[frame], right[frame]]);
        }
    }
    if !pcm.iter().all(|sample| sample.is_finite()) {
        return Err(CliError::new("S6a renderer produced non-finite PCM"));
    }
    Ok(Rendered {
        pcm,
        timings,
        faults: driver.processor().fault_counters(),
        simulation,
    })
}

fn step_simulation(
    runner: &mut dyn SimulationRunner,
    prepared: &PreparedFixture,
    rendered_frames: u64,
    next_ticks: &mut [u64; 3],
    telemetry: &mut SimulationTelemetry,
) -> Result<()> {
    let rates = [60_u64, 15, 5];
    let any_due = (0..3).any(|pass| {
        rendered_frames
            >= next_ticks[pass]
                .saturating_mul(u64::from(SAMPLE_RATE))
                .div_ceil(rates[pass])
    });
    if any_due {
        runner.update_inputs(&simulation_update(prepared, rendered_frames));
    }
    for pass in 0..3 {
        while rendered_frames
            >= next_ticks[pass]
                .saturating_mul(u64::from(SAMPLE_RATE))
                .div_ceil(rates[pass])
        {
            let started = Instant::now();
            let result = match pass {
                0 => runner.run_direct(),
                1 => runner.run_pathing(),
                _ => runner.run_reflections(),
            };
            let duration = elapsed_ns(started);
            match pass {
                0 => telemetry.direct.record(result, duration),
                1 => telemetry.pathing.record(result, duration),
                _ => telemetry.reflections.record(result, duration),
            }
            if result.is_err() {
                return Err(CliError::new(format!(
                    "{} simulation pass failed",
                    ["direct", "pathing", "reflections"][pass]
                )));
            }
            next_ticks[pass] += 1;
        }
    }
    Ok(())
}

fn build_graph(
    prepared: &PreparedFixture,
    baked: &BakedProbeBatch,
) -> Result<(
    fightbox_steam_audio::SteamAudioSimulationRunner,
    RuntimeGraph,
)> {
    let descriptors: Vec<_> = prepared
        .fixture
        .sources
        .iter()
        .map(|source| MultiSourceDescriptor::at(initial_position(source)))
        .collect();
    let (runner, backend) = build_multi_source_session(
        &prepared.mesh,
        baked,
        audio_config(),
        prepared.simulation,
        &descriptors,
    )
    .map_err(|error| CliError::new(format!("cannot build S6a session: {error}")))?;
    let snapshot = PropagationSnapshot {
        sequence: 1,
        simulated_at_ns: u64::MAX,
        sources: std::array::from_fn(|index| SourcePropagation {
            active: index < prepared.sources.len(),
            target_delay_samples: 0.0,
            left_gain: 1.0,
            right_gain: 1.0,
        }),
    };
    let (_writer, reader) = SnapshotPublication::new(snapshot);
    let mut graph = RuntimeGraph::new_with_backend(
        engine_config(prepared.sources.len()),
        reader,
        Box::new(backend),
    )
    .map_err(|error| CliError::new(format!("cannot build runtime graph: {error:?}")))?;
    graph.set_listener_state(listener_state_at(&prepared.fixture, 0));
    for (index, source) in prepared.sources.iter().enumerate() {
        graph
            .set_source(index, &source.profile, SceneCalibration::default())
            .map_err(|error| {
                CliError::new(format!("cannot configure source {index}: {error:?}"))
            })?;
    }
    Ok((runner, graph))
}

fn initial_simulation(runner: &mut dyn SimulationRunner) -> Result<()> {
    runner
        .run_direct()
        .and_then(|_| runner.run_pathing())
        .and_then(|_| runner.run_reflections())
        .map_err(|error| CliError::new(format!("initial simulation failed: {error:?}")))
}

fn simulation_update(prepared: &PreparedFixture, rendered_frames: u64) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    for (index, source) in prepared.fixture.sources.iter().enumerate() {
        let (position, velocity) = motion_at(source, rendered_frames);
        sources[index] = SourceMotion {
            active: true,
            pose: Pose {
                position,
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            linear_velocity_mps: velocity,
        };
    }
    SimulationUpdate {
        listener: listener_state_at(&prepared.fixture, rendered_frames),
        sources,
    }
}

fn motion_at(source: &FixtureSource, rendered_frames: u64) -> (EnuVector3, EnuVector3) {
    let Some(trajectory) = &source.trajectory else {
        return (
            to_enu(source.position_m.expect("validated static source")),
            EnuVector3::default(),
        );
    };
    trajectory_motion_at(trajectory, rendered_frames)
}

fn trajectory_motion_at(trajectory: &Trajectory, rendered_frames: u64) -> (EnuVector3, EnuVector3) {
    let mut remaining = rendered_frames as f64 / f64::from(SAMPLE_RATE) * trajectory.speed_mps;
    for segment in trajectory.waypoints_m.windows(2) {
        let delta = sub(segment[1], segment[0]);
        let distance = length(delta);
        if remaining < distance {
            let unit = scale(delta, 1.0 / distance);
            return (
                to_enu(add(segment[0], scale(unit, remaining))),
                to_enu(scale(unit, trajectory.speed_mps)),
            );
        }
        remaining -= distance;
    }
    (
        to_enu(*trajectory.waypoints_m.last().unwrap()),
        EnuVector3::default(),
    )
}

fn prepare_fixture(
    path: &Path,
    reflection_effect: ReflectionEffectConfig,
    fixture_use: FixtureUse,
) -> Result<PreparedFixture> {
    let bytes = std::fs::read(path)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", path.display())))?;
    let fixture: Fixture = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::new(format!("invalid S6a fixture: {error}")))?;
    validate_fixture(&fixture, fixture_use)?;
    let mut sources = Vec::new();
    for source in &fixture.sources {
        let descriptor_path = repo_root()
            .join("fixtures/assets")
            .join(format!("{}.json", source.asset_id));
        let descriptor_bytes = std::fs::read(&descriptor_path).map_err(|error| {
            CliError::new(format!(
                "cannot read {}: {error}",
                descriptor_path.display()
            ))
        })?;
        let descriptor = AssetDescriptor::parse(
            std::str::from_utf8(&descriptor_bytes)
                .map_err(|error| CliError::new(format!("asset is not UTF-8: {error}")))?,
        )
        .map_err(CliError::new)?;
        if descriptor.asset_id != source.asset_id {
            return Err(CliError::new(format!(
                "source {} asset identity mismatch",
                source.id
            )));
        }
        let resolved = descriptor.resolve().map_err(CliError::new)?;
        let (signal, analysis) = resolved.regenerate_mono().map_err(CliError::new)?;
        sources.push(PreparedSource {
            id: source.id.clone(),
            asset_id: source.asset_id.clone(),
            descriptor_hash: sha256_hex(&descriptor_bytes),
            signal: signal.samples,
            profile: SourceProfile {
                id: SourceId::new(&source.id),
                pose: Pose {
                    position: initial_position(source),
                    forward: EnuVector3::new(0.0, 1.0, 0.0),
                    up: EnuVector3::new(0.0, 0.0, 1.0),
                },
                reference_level: ReferenceLevel::SplAtOneMeter {
                    db_spl: source.reference_level.db_spl as f32,
                },
                asset_analysis: analysis.analysis().clone(),
                extent: ExtentDescriptor::Point,
                max_speed_mps: source
                    .trajectory
                    .as_ref()
                    .map_or(0.0, |value| value.max_speed_mps as f32),
            },
        });
    }
    let duration = fixture
        .sources
        .iter()
        .filter_map(|source| source.trajectory.as_ref())
        .chain(fixture.listener.trajectory.iter())
        .map(trajectory_duration)
        .reduce(f64::max)
        .ok_or_else(|| {
            CliError::new("fixture requires a source or listener trajectory to set its timeline")
        })?;
    let timeline_frames = (duration * f64::from(SAMPLE_RATE)).ceil() as usize;
    let mesh = build_mesh(&fixture.geometry)?;
    let probes = ProbeVolume {
        min_enu_m: to_steam_enu(fixture.simulation.probe_volume.min_m),
        max_enu_m: to_steam_enu(fixture.simulation.probe_volume.max_m),
        spacing_m: fixture.simulation.probe_volume.spacing_m as f32,
        height_above_floor_m: fixture.simulation.probe_generation.height_m as f32,
    };
    let simulation = simulation_config(&fixture, reflection_effect);
    Ok(PreparedFixture {
        fixture,
        fixture_hash: sha256_hex(&bytes),
        sources,
        mesh,
        probes,
        simulation,
        timeline_frames,
    })
}

fn validate_fixture(fixture: &Fixture, fixture_use: FixtureUse) -> Result<()> {
    if fixture.schema_version != "fightbox.fixture.s6a.v1"
        || !matches!(fixture.gate.as_str(), "S5" | "S6A")
    {
        return Err(CliError::new("expected a fightbox.fixture.s6a.v1 fixture"));
    }
    let moving_sources = fixture
        .sources
        .iter()
        .filter(|source| source.trajectory.is_some())
        .count();
    match fixture_use {
        FixtureUse::PhaseBS6a
            if fixture.gate != "S6A"
                || fixture.sources.len() != 4
                || moving_sources != 1
                || fixture.listener.trajectory.is_some()
                || fixture.listener.position_m.is_none() =>
        {
            return Err(CliError::new(
                "phase-b S6a requires four sources, exactly one moving source, and a static listener",
            ));
        }
        FixtureUse::City
            if fixture.sources.is_empty()
                || fixture.sources.len() > MAX_ACTIVE_SOURCES
                || moving_sources > 1 =>
        {
            return Err(CliError::new(
                "city render requires 1..=8 sources with at most one moving source",
            ));
        }
        _ => {}
    }
    for source in &fixture.sources {
        if (source.position_m.is_some() as u8 + source.trajectory.is_some() as u8) != 1
            || !source.reference_level.db_spl.is_finite()
            || source.reference_level.mode != ReferenceMode::SplAtOneMeter
            || source
                .position_m
                .is_some_and(|position| !vec3_is_finite(position))
        {
            return Err(CliError::new(format!(
                "source {} violates the S6a source contract",
                source.id
            )));
        }
        if let Some(trajectory) = &source.trajectory {
            validate_trajectory(trajectory, &format!("source {}", source.id))?;
        }
    }
    if (fixture.listener.position_m.is_some() as u8 + fixture.listener.trajectory.is_some() as u8)
        != 1
    {
        return Err(CliError::new(
            "listener requires exactly one of position_m or trajectory",
        ));
    }
    if let Some(trajectory) = &fixture.listener.trajectory {
        validate_trajectory(trajectory, "listener")?;
    }
    if fixture
        .listener
        .position_m
        .is_some_and(|position| !vec3_is_finite(position))
        || !vec3_is_finite(fixture.listener.forward_enu)
        || !vec3_is_finite(fixture.listener.up_enu)
    {
        return Err(CliError::new(
            "listener position and orientation must be finite",
        ));
    }
    if !fixture.simulation.direct.distance_attenuation
        || !fixture.simulation.direct.occlusion
        || !fixture.simulation.reflections.enabled
        || !fixture.simulation.pathing.enabled
        || fixture.simulation.probe_volume.kind != ProbeVolumeKind::Box
        || fixture.simulation.probe_generation.kind != ProbeGenerationKind::UniformFloor
    {
        return Err(CliError::new("S6a simulation features are not enabled"));
    }
    let _ = (
        &fixture.coordinate_frame,
        &fixture.kernel,
        &fixture.expected,
        &fixture.simulation.path_bake,
        &fixture.simulation.pathing.runtime_order,
    );
    Ok(())
}

fn validate_trajectory(trajectory: &Trajectory, owner: &str) -> Result<()> {
    if trajectory.waypoints_m.len() < 2
        || !trajectory.speed_mps.is_finite()
        || !trajectory.max_speed_mps.is_finite()
        || trajectory.speed_mps <= 0.0
        || trajectory.speed_mps > trajectory.max_speed_mps
        || trajectory
            .waypoints_m
            .iter()
            .flatten()
            .any(|coordinate| !coordinate.is_finite())
    {
        return Err(CliError::new(format!(
            "{owner} violates its trajectory speed or waypoint contract"
        )));
    }
    if trajectory
        .waypoints_m
        .windows(2)
        .any(|segment| length(sub(segment[1], segment[0])) <= f64::EPSILON)
    {
        return Err(CliError::new(format!(
            "{owner} trajectory contains a zero-length segment"
        )));
    }
    Ok(())
}

fn vec3_is_finite(value: [f64; 3]) -> bool {
    value.into_iter().all(f64::is_finite)
}

fn build_mesh(geometry: &FixtureGeometry) -> Result<SceneMesh> {
    let mut names = Vec::<String>::new();
    let mut materials = Vec::new();
    let mut material_indices = Vec::new();
    let mut triangles = Vec::new();
    for triangle in &geometry.triangles {
        if triangle
            .indices
            .iter()
            .any(|index| *index >= geometry.vertices_m.len())
        {
            return Err(CliError::new("fixture triangle index is out of bounds"));
        }
        let material = geometry
            .materials
            .get(&triangle.material)
            .ok_or_else(|| CliError::new(format!("unknown material {}", triangle.material)))?;
        let index = if let Some(index) = names.iter().position(|name| name == &triangle.material) {
            index
        } else {
            names.push(triangle.material.clone());
            materials.push(AcousticMaterial {
                absorption: f32s(material.absorption),
                scattering: material.scattering as f32,
                transmission: f32s(material.transmission),
            });
            materials.len() - 1
        };
        triangles.push(triangle.indices.map(|value| value as i32));
        material_indices.push(index as i32);
    }
    Ok(SceneMesh {
        vertices_enu_m: geometry
            .vertices_m
            .iter()
            .copied()
            .map(to_steam_enu)
            .collect(),
        triangles,
        material_indices,
        materials,
    })
}

fn reflection_effect_name(reflection_effect: ReflectionEffectConfig) -> &'static str {
    if reflection_effect == ReflectionEffectConfig::PARAMETRIC {
        "parametric"
    } else {
        "convolution"
    }
}

fn simulation_config(
    fixture: &Fixture,
    reflection_effect: ReflectionEffectConfig,
) -> S3SimulationConfig {
    // Steam Audio's convolution IR ray tracer does not expose a seed and
    // produces slightly different IR samples in fresh processes. Parametric
    // reflections exercise the same cadenced simulation pass and shared
    // reflection mixer while yielding the byte-stable offline PCM required by
    // the B2 proof contract.
    S3SimulationConfig {
        max_occlusion_samples: fixture.simulation.direct.occlusion_samples.unwrap_or(64) as i32,
        direct_occlusion: DirectOcclusionMode::Raycast,
        reflection_rays: fixture.simulation.reflections.rays.unwrap_or(4_096) as i32,
        reflection_bounces: fixture.simulation.reflections.bounces.unwrap_or(2) as i32,
        reflection_duration_s: fixture.simulation.reflections.duration_s.unwrap_or(1.0) as f32,
        reflection_effect,
        pathing_order: fixture.simulation.pathing.order.unwrap_or(2) as i32,
        validate_paths: fixture.simulation.pathing.validation.unwrap_or(true),
        find_alternate_paths: fixture.simulation.pathing.alternate_paths.unwrap_or(true),
        trace_path_validation: false,
        ..S3SimulationConfig::default()
    }
}

fn bake_fixture(prepared: &PreparedFixture) -> Result<BakedProbeBatch> {
    bake_s3(&S3BakeRequest {
        mesh: prepared.mesh.clone(),
        probes: prepared.probes,
        pathing: PathBakeConfig::default(),
    })
    .map_err(|error| CliError::new(format!("S6a path bake failed: {error}")))
}

fn identity(prepared: &PreparedFixture, baked: &BakedProbeBatch) -> IdentityReport {
    IdentityReport {
        fixture_id: prepared.fixture.fixture_id.clone(),
        fixture_sha256: prepared.fixture_hash.clone(),
        assets: prepared
            .sources
            .iter()
            .enumerate()
            .map(|(source_index, source)| AssetIdentity {
                source_index,
                source_id: source.id.clone(),
                asset_id: source.asset_id.clone(),
                descriptor_sha256: source.descriptor_hash.clone(),
            })
            .collect(),
        probe_batch_sha256: baked.metadata.content_sha256.clone(),
    }
}

fn listener_state_at(fixture: &Fixture, rendered_frames: u64) -> ListenerState {
    let (position, velocity) = if let Some(position) = fixture.listener.position_m {
        (to_enu(position), EnuVector3::default())
    } else {
        trajectory_motion_at(
            fixture
                .listener
                .trajectory
                .as_ref()
                .expect("validated listener trajectory"),
            rendered_frames,
        )
    };
    ListenerState {
        pose: Pose {
            position,
            forward: to_enu(fixture.listener.forward_enu),
            up: to_enu(fixture.listener.up_enu),
        },
        linear_velocity_mps: velocity,
    }
}

fn initial_position(source: &FixtureSource) -> EnuVector3 {
    source
        .position_m
        .map(to_enu)
        .unwrap_or_else(|| to_enu(source.trajectory.as_ref().unwrap().waypoints_m[0]))
}

fn trajectory_duration(value: &Trajectory) -> f64 {
    value
        .waypoints_m
        .windows(2)
        .map(|segment| length(sub(segment[1], segment[0])))
        .sum::<f64>()
        / value.speed_mps
}

fn audio_config() -> AudioConfig {
    AudioConfig {
        sample_rate_hz: SAMPLE_RATE as i32,
        frame_size: BLOCK_SIZE as i32,
    }
}

fn engine_config(source_count: usize) -> EngineConfig {
    EngineConfig {
        sample_rate_hz: SAMPLE_RATE,
        block_size_frames: BLOCK_SIZE as u32,
        max_active_sources: source_count as u8,
        ..EngineConfig::default()
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn require_linked(command: &str) -> Result<()> {
    if fightbox_steam_audio::backend_availability()
        .to_json()
        .contains(r#""status":"available""#)
    {
        Ok(())
    } else {
        Err(CliError::new(format!(
            "{command} requires --features linked-sdk and STEAM_AUDIO_SDK_DIR"
        )))
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn to_enu(value: [f64; 3]) -> EnuVector3 {
    EnuVector3::new(value[0] as f32, value[1] as f32, value[2] as f32)
}

fn to_steam_enu(value: [f64; 3]) -> SteamEnuVector3 {
    SteamEnuVector3::new(value[0] as f32, value[1] as f32, value[2] as f32)
}

fn f32s(value: [f64; 3]) -> [f32; 3] {
    [value[0] as f32, value[1] as f32, value[2] as f32]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(value: [f64; 3], factor: f64) -> [f64; 3] {
    [value[0] * factor, value[1] * factor, value[2] * factor]
}

fn length(value: [f64; 3]) -> f64 {
    (value[0] * value[0] + value[1] * value[1] + value[2] * value[2]).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!(
            "../../../fixtures/s6a-four-sources/fixture.json"
        ))
        .unwrap()
    }

    #[test]
    fn repository_fixture_is_valid() {
        validate_fixture(&fixture(), FixtureUse::PhaseBS6a).unwrap();
    }

    fn walk_fixture() -> Fixture {
        serde_json::from_str(include_str!(
            "../../../fixtures/city/chicago-walk/fixture.json"
        ))
        .unwrap()
    }

    #[test]
    fn walk_fixture_parses_and_validates_for_city_render() {
        let fixture = walk_fixture();
        validate_fixture(&fixture, FixtureUse::City).unwrap();
        assert_eq!(fixture.sources.len(), 1);
        assert!(fixture.listener.trajectory.is_some());
        assert!(validate_fixture(&fixture, FixtureUse::PhaseBS6a).is_err());
    }

    #[test]
    fn city_validation_rejects_more_than_eight_or_two_moving_sources() {
        let mut too_many = walk_fixture();
        while too_many.sources.len() <= MAX_ACTIVE_SOURCES {
            let mut source = too_many.sources[0].clone();
            source.id = format!("source-{}", too_many.sources.len());
            too_many.sources.push(source);
        }
        assert!(validate_fixture(&too_many, FixtureUse::City).is_err());

        let mut two_moving = walk_fixture();
        let trajectory = two_moving.listener.trajectory.clone().unwrap();
        two_moving.sources[0].position_m = None;
        two_moving.sources[0].trajectory = Some(trajectory.clone());
        let mut second = two_moving.sources[0].clone();
        second.id = "second-moving".into();
        second.trajectory = Some(trajectory);
        two_moving.sources.push(second);
        assert!(validate_fixture(&two_moving, FixtureUse::City).is_err());
    }

    #[test]
    fn listener_trajectory_stepping_is_byte_identical_across_two_renders() {
        let fixture = walk_fixture();
        let timeline_frames = (trajectory_duration(fixture.listener.trajectory.as_ref().unwrap())
            * f64::from(SAMPLE_RATE))
        .ceil() as u64;
        let render = || {
            (0..timeline_frames)
                .step_by(BLOCK_SIZE)
                .flat_map(|frame| {
                    let state = listener_state_at(&fixture, frame);
                    [
                        state.pose.position.east_m.to_bits(),
                        state.pose.position.north_m.to_bits(),
                        state.pose.position.up_m.to_bits(),
                        state.linear_velocity_mps.east_m.to_bits(),
                        state.linear_velocity_mps.north_m.to_bits(),
                        state.linear_velocity_mps.up_m.to_bits(),
                    ]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(render(), render());
    }

    #[test]
    fn listening_gain_is_shared_and_targets_minus_three_dbfs() {
        let mix = vec![0.25_f32, -0.5, 0.1];
        let stem = vec![0.75_f32, -0.125];
        let bundle_peak = mix
            .iter()
            .chain(&stem)
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max);
        let gain = listening_gain(bundle_peak, -3.0);
        let normalized_peak = apply_gain(&mix, gain)
            .into_iter()
            .chain(apply_gain(&stem, gain))
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        assert!((amplitude_dbfs(normalized_peak).unwrap() - -3.0).abs() < 1.0e-5);
        assert_eq!(mix, vec![0.25, -0.5, 0.1]);
    }

    #[test]
    fn timeline_covers_the_complete_trajectory() {
        let fixture = fixture();
        let moving = fixture
            .sources
            .iter()
            .find_map(|source| source.trajectory.as_ref())
            .unwrap();
        assert!((trajectory_duration(moving) - 16.0 / 15.0).abs() < 1.0e-12);
    }

    #[test]
    fn fixed_audio_contract_is_48k_128_and_four_sources() {
        assert_eq!(audio_config().sample_rate_hz, 48_000);
        assert_eq!(audio_config().frame_size, 128);
        assert_eq!(engine_config(4).max_active_sources, 4);
        assert_eq!(engine_config(8).max_active_sources, 8);
    }
}
