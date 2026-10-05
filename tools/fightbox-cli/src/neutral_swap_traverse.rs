//! Linked desktop callback traversal for the four-cell route.
//!
//! This is deliberately a small harness around the public neutral session and
//! `CellStreamManager` APIs. It is not a device, iOS, hardware, or listening
//! test: the callback is invoked synchronously by this CLI and its fixed banks
//! are measured as desktop linked output.

use std::path::{Path, PathBuf};
use std::time::Instant;

use fightbox_api::{
    Directivity, EnuVector3, ExtentDescriptor, ListenerState, Pose, ReferenceLevel, SourceId,
    SourceProfile,
};
use fightbox_evidence::sha256_hex;
use fightbox_runtime::backend::{
    SimulationRunner, SimulationUpdate, SourceMotion, SpatialBackendRenderGraph,
    SpatialBackendSourceBlock, SpatialOutputMetadata, SpatialOutputValidity,
    SpatialPropagationRenderBlock,
};
use fightbox_runtime::{
    CellIdentity, CellPrepareEstimate, CellStreamManager, CompletePreparation, FreshMemorySample,
    MAX_ACTIVE_SOURCES, PrepareAdmission, RunTimingHistogram,
};
use fightbox_steam_audio::{
    AudioConfig, BackendError, BakedProbeBatch, DirectOcclusionMode, MultiSourceDescriptor,
    ReflectionEffectConfig, S3SimulationConfig, SceneMesh, SteamAudioSpatialRenderGraph,
    SteamAudioSpatialSimulationRunner, build_spatial_multi_source_session,
};
use fightbox_world::{
    CITY_BAKE_V2_CAPABILITY, CityRouteCellRecord, CityRouteManifest, read_package_with_capabilities,
};
use serde_json::{Value, json};

use crate::NeutralSwapTraverseArgs;
use crate::asset::AssetDescriptor;
use crate::atomicio::{self, AtomicDir};
use crate::city;
use crate::city_route;
use crate::error::{CliError, Result};

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_SIZE: usize = 128;
const ROUTE_HZ: f64 = 15.0;
const CONTROL_HZ: u64 = 60;
const CONTROL_INTERVAL_FRAMES: u64 = SAMPLE_RATE as u64 / CONTROL_HZ;
const CONTROLS_PER_ROUTE_SAMPLE: u64 = 4;
const EXPECTED_WARMUP_BLOCKS: u64 = 64;
const EXPECTED_CROSSFADE_BLOCKS: u64 = 8;
const SPEED_MPS: f64 = 7.5;
const LEVEL_GATE_DB: f64 = 1.0;
const CALLBACK_P99_LIMIT_MS: f64 = 1.33;
const CALLBACK_P99_9_LIMIT_MS: f64 = 2.13;
const SOURCE_ASSET_ID: &str = "s3-calibrated-pink";
const SOURCE_DB_SPL: f32 = 85.0;
const EVENT_ID: u64 = 1;
const EVENT_EMISSION_FRAME: u64 = 512;
const EVENT_SEEK_FRAME: u64 = 777;

// The authored route is four 100 m legs. At 7.5 m/s and 48 kHz every route
// sample is exactly 3,200 frames and every ownership switch is a callback
// boundary. These are absolute city ENU positions, never cell-local values.
const WAYPOINTS: [[f64; 3]; 5] = [
    [192.5, 192.5, 1.5],
    [292.5, 192.5, 1.5],
    [292.5, 292.5, 1.5],
    [192.5, 292.5, 1.5],
    [192.5, 392.5, 1.5],
];
const SOURCE_CITY: [f64; 3] = [242.0, 242.0, 1.5];

#[derive(Clone)]
struct CellArtifact {
    record: CityRouteCellRecord,
    package: PathBuf,
    bake: PathBuf,
    mesh: SceneMesh,
    baked: BakedProbeBatch,
}

struct SessionParts {
    simulation: SteamAudioSpatialSimulationRunner,
    graph: SteamAudioSpatialRenderGraph,
    source_signal: Vec<f32>,
}

#[derive(Default)]
struct CallbackStats {
    callbacks: u64,
    backend_failures: u64,
    silent_blocks: u64,
    direction_checks: u64,
    direction_failures: u64,
    event_checks: u64,
    event_failures: u64,
    timeline_restarts: u64,
    control_updates: u64,
    route_control_updates: u64,
    pathing_updates: u64,
    reflection_updates: u64,
    warmup_blocks: u64,
    crossfade_blocks: u64,
    transition_metadata_failures: u64,
    third_world_refusals: u64,
    tail_retiring_blocks: u64,
    callback_ns: Vec<u64>,
    levels_by_frame: Vec<(u64, f64)>,
    metadata_generations: Vec<u64>,
    validities: Vec<&'static str>,
}

struct CallbackResourceObservation {
    sample_count: u64,
    p50_ms: f64,
    p99_ms: f64,
    p99_9_ms: f64,
    max_ms: f64,
    peak_rss_mib: f64,
}

fn callback_resource_observation(samples_ns: &[u64]) -> Result<CallbackResourceObservation> {
    let mut histogram = RunTimingHistogram::default();
    for &sample_ns in samples_ns {
        histogram.record(sample_ns);
    }
    let milliseconds = |duration_ns: u64| duration_ns as f64 / 1_000_000.0;
    let percentile_ms = |percentile| {
        histogram
            .percentile_ns(percentile)
            .map(milliseconds)
            .ok_or_else(|| CliError::new("callback resource observation has no timing samples"))
    };
    Ok(CallbackResourceObservation {
        sample_count: histogram.len(),
        p50_ms: percentile_ms(50.0)?,
        p99_ms: percentile_ms(99.0)?,
        p99_9_ms: percentile_ms(99.9)?,
        max_ms: histogram
            .max_ns()
            .map(milliseconds)
            .ok_or_else(|| CliError::new("callback resource observation has no maximum"))?,
        peak_rss_mib: process_peak_rss_mib()?,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_peak_rss_mib() -> Result<f64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for exactly one `libc::rusage`.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return Err(CliError::new(format!(
            "cannot read process peak RSS: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: a zero status means `getrusage` initialized the complete structure.
    let usage = unsafe { usage.assume_init() };
    #[cfg(target_os = "macos")]
    let peak_bytes = usage.ru_maxrss as f64;
    #[cfg(target_os = "linux")]
    let peak_bytes = usage.ru_maxrss as f64 * 1024.0;
    Ok(peak_bytes / (1024.0 * 1024.0))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_peak_rss_mib() -> Result<f64> {
    Err(CliError::new(
        "process peak RSS observation is unsupported on this platform",
    ))
}

pub(crate) fn run(args: NeutralSwapTraverseArgs) -> Result<()> {
    require_linked()?;
    let route_path = canonical_file(&args.route_manifest, "route manifest")?;
    let artifact_root = canonical_dir(&args.artifact_root, "artifact root")?;
    let oracle_package = canonical_dir(&args.oracle_package, "oracle package")?;
    let oracle_bake = canonical_dir(&args.oracle_bake, "oracle bake")?;
    let output = atomicio::validate_output_path(&args.output)?;

    let route_bytes = std::fs::read(&route_path).map_err(|e| {
        CliError::new(format!(
            "cannot read route manifest {}: {e}",
            route_path.display()
        ))
    })?;
    let route = CityRouteManifest::from_bytes(&route_bytes)
        .map_err(|e| CliError::new(format!("invalid route manifest: {e}")))?;
    if route.cells.len() != 4 || route.four_cell_fixture.is_none() {
        return Err(CliError::new(
            "neutral-swap-traverse requires a validated four-cell route manifest",
        ));
    }
    if !route.production_eligible {
        return Err(CliError::new(
            "neutral-swap-traverse requires a production-eligible route manifest",
        ));
    }

    // Load and verify all four package/bake pairs before constructing a linked
    // session. This is an actual artifact pass, not a route-manifest-only test.
    let cells = load_four_cells(&route, &artifact_root)?;
    let source = load_source_signal()?;
    let config = simulation_config();
    let timeline_frames = route_timeline_frames();

    // The oracle runs first and is fully dropped before the first streamed
    // world is prepared. No oracle graph is reused as a streamed cell.
    let (oracle_levels, oracle_stats) = render_oracle(
        &oracle_package,
        &oracle_bake,
        &source,
        config,
        timeline_frames,
    )?;

    let first = &cells[0];
    let descriptor = descriptor_for_offset(first.record.local_to_city_enu_m);
    let (simulation, render) = build_spatial_multi_source_session(
        &first.mesh,
        &first.baked,
        audio_config(),
        config,
        &[descriptor],
        &[1],
        2,
    )
    .map_err(backend_error)?;
    let mut session = make_session(
        simulation,
        render,
        source.1.clone(),
        first.record.local_to_city_enu_m,
        0,
    )?;
    let mut manager = CellStreamManager::new(
        cell_identity(first),
        (),
        first.record.installed_size.actual_installed_bytes,
        Default::default(),
    );
    let mut streamed_stats = CallbackStats::default();
    // Continue with silent callback blocks after the route so the last old
    // environmental tail is observed and collected on the control side.
    render_blocks(
        &mut session,
        &source.0,
        timeline_frames.saturating_add((BLOCK_SIZE * 4096) as u64),
        timeline_frames,
        Some((&cells, &mut manager)),
        &mut streamed_stats,
    )?;
    let streamed_levels = streamed_stats.levels_by_frame.clone();
    let gate = compare_levels(&oracle_levels, &streamed_levels, timeline_frames);
    let swaps = streamed_stats
        .metadata_generations
        .windows(2)
        .filter(|pair| pair[0] != pair[1])
        .count();
    let direction_passed =
        streamed_stats.direction_failures == 0 && streamed_stats.direction_checks > 0;
    let expected_event_samples = timeline_frames.saturating_sub(EVENT_EMISSION_FRAME);
    let event_passed = streamed_stats.event_failures == 0
        && streamed_stats.timeline_restarts == 0
        && streamed_stats.event_checks == expected_event_samples;
    let oracle_passed = oracle_stats.callbacks == timeline_frames.div_ceil(BLOCK_SIZE as u64)
        && oracle_stats.backend_failures == 0
        && oracle_stats.silent_blocks == 0;
    let swaps_passed = swaps == 3;
    let third_world_refusal_passed = streamed_stats.third_world_refusals == 3;
    let output_validity_passed =
        streamed_stats.backend_failures == 0 && streamed_stats.silent_blocks == 0;
    let tail_complete =
        manager.telemetry().tail_retiring.is_none() && streamed_stats.tail_retiring_blocks > 0;
    let expected_route_controls = timeline_frames.div_ceil(CONTROL_INTERVAL_FRAMES);
    let control_passed = streamed_stats.route_control_updates == expected_route_controls
        && (CONTROL_HZ as f64 / ROUTE_HZ) == CONTROLS_PER_ROUTE_SAMPLE as f64;
    let transition_timing_passed = streamed_stats.warmup_blocks == EXPECTED_WARMUP_BLOCKS * 3
        && streamed_stats.crossfade_blocks == EXPECTED_CROSSFADE_BLOCKS * 3
        && streamed_stats.transition_metadata_failures == 0;
    let executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| CliError::new(format!("cannot resolve harness executable: {error}")))?;
    let executable_bytes = std::fs::read(&executable)
        .map_err(|error| CliError::new(format!("cannot hash harness executable: {error}")))?;
    let resources = callback_resource_observation(&streamed_stats.callback_ns)?;
    let resources_passed =
        resources.p99_ms < CALLBACK_P99_LIMIT_MS && resources.p99_9_ms < CALLBACK_P99_9_LIMIT_MS;
    let report = json!({
        "schema_version": "fightbox.phase-b.neutral-swap-traverse.v2",
        "evidence_class": "desktop_linked_callback_harness",
        "status": if gate.passed && oracle_passed && swaps_passed && direction_passed && event_passed && output_validity_passed && tail_complete && third_world_refusal_passed && control_passed && transition_timing_passed && resources_passed { "passed" } else { "failed" },
        "provenance": {
            "source_sha256": sha256_hex(include_bytes!("neutral_swap_traverse.rs")),
            "executable": path_string(&executable),
            "executable_sha256": sha256_hex(&executable_bytes),
            "command": [
                "phase-b", "neutral-swap-traverse",
                "--route-manifest", path_string(&route_path),
                "--artifact-root", path_string(&artifact_root),
                "--oracle-package", path_string(&oracle_package),
                "--oracle-bake", path_string(&oracle_bake),
                "--output", path_string(&output),
            ],
        },
        "route": {
            "manifest": path_string(&route_path),
            "manifest_sha256": sha256_hex(&route_bytes),
            "route_id": route.route_id,
            "city_id": route.city_id,
            "production_eligible": route.production_eligible,
            "artifact_root": path_string(&artifact_root),
            "cells": cells.iter().map(|cell| json!({
                "cell_id": cell.record.cell_id,
                "package": path_string(&cell.package),
                "bake": path_string(&cell.bake),
                "package_manifest_sha256": cell.record.world.manifest_sha256,
                "bake_sidecar_sha256": cell.record.city_bake.completed.as_ref().map(|b| b.sidecar_sha256.clone()),
                "probe_batch_sha256": cell.record.city_bake.completed.as_ref().map(|b| b.probe_batch_sha256.clone()),
            })).collect::<Vec<_>>(),
        },
        "oracle": {
            "package": path_string(&oracle_package),
            "bake": path_string(&oracle_bake),
            "pass": oracle_passed,
            "callbacks": oracle_stats.callbacks,
            "backend_failures": oracle_stats.backend_failures,
            "dropped_before_streamed_pass": true,
        },
        "source_timeline": source_timeline_json(&source),
        "callback": {
            "path": "public SpatialBackendRenderGraph::render_spatial_block on SteamAudioSpatialRenderGraph",
            "host": "desktop linked CLI; synchronous callback invocation; no device or listener claim",
            "sample_rate_hz": SAMPLE_RATE,
            "block_size_frames": BLOCK_SIZE,
            "callbacks": streamed_stats.callbacks,
            "control_updates": streamed_stats.control_updates,
            "route_control_updates": streamed_stats.route_control_updates,
            "pathing_updates": streamed_stats.pathing_updates,
            "reflection_updates": streamed_stats.reflection_updates,
            "backend_failures": streamed_stats.backend_failures,
            "silent_discontinuity_blocks": streamed_stats.silent_blocks,
            "successful_swaps": swaps,
            "expected_swaps": 3,
            "single_runtime_graph": true,
            "pre_crossfade_warmup_blocks_per_swap": EXPECTED_WARMUP_BLOCKS,
            "observed_warmup_blocks": streamed_stats.warmup_blocks,
            "crossfade_blocks_per_swap": EXPECTED_CROSSFADE_BLOCKS,
            "observed_crossfade_blocks": streamed_stats.crossfade_blocks,
            "transition_metadata_failures": streamed_stats.transition_metadata_failures,
            "third_world_refusals": streamed_stats.third_world_refusals,
            "tail_retiring_blocks": streamed_stats.tail_retiring_blocks,
        },
        "resources": {
            "platform": format!("desktop-{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "scope": "single process peak across oracle plus streamed passes; callback timing covers streamed public render_spatial_block calls",
            "passed": resources_passed,
            "callback_sample_count": resources.sample_count,
            "callback_p50_ms": resources.p50_ms,
            "callback_p99_ms": resources.p99_ms,
            "callback_p99_limit_ms": CALLBACK_P99_LIMIT_MS,
            "callback_p999_ms": resources.p99_9_ms,
            "callback_p999_limit_ms": CALLBACK_P99_9_LIMIT_MS,
            "callback_max_ms": resources.max_ms,
            "peak_rss_mib": resources.peak_rss_mib,
            "timing_method": "Instant wall duration accumulated outside the public synchronous callback and summarized by conservative RunTimingHistogram buckets",
            "peak_rss_method": "getrusage(RUSAGE_SELF).ru_maxrss sampled after oracle and streamed traversal",
        },
        "assertions": {
            "control_cadence": {"passed": control_passed, "control_hz": CONTROL_HZ, "route_hz": ROUTE_HZ, "controls_per_route_sample": CONTROLS_PER_ROUTE_SAMPLE, "expected_route_controls": expected_route_controls, "observed_route_controls": streamed_stats.route_control_updates},
            "transition_timing": {"passed": transition_timing_passed, "warmup_blocks_per_swap": EXPECTED_WARMUP_BLOCKS, "observed_warmup_blocks": streamed_stats.warmup_blocks, "crossfade_blocks_per_swap": EXPECTED_CROSSFADE_BLOCKS, "observed_crossfade_blocks": streamed_stats.crossfade_blocks, "metadata_identity_failures": streamed_stats.transition_metadata_failures},
            "swap_count": {"passed": swaps_passed, "expected": 3, "observed": swaps},
            "output_validity": {"passed": output_validity_passed, "backend_failures": streamed_stats.backend_failures, "invalid_blocks": streamed_stats.silent_blocks},
            "direction": {"passed": direction_passed, "checks": streamed_stats.direction_checks, "failures": streamed_stats.direction_failures, "rule": "presentation feed direction equals the canonical city-frame source minus listener control pose"},
            "event": {"passed": event_passed, "checks": streamed_stats.event_checks, "expected_checks": expected_event_samples, "failures": streamed_stats.event_failures, "event_id": EVENT_ID, "emission_frame": EVENT_EMISSION_FRAME, "program_seek_frame": EVENT_SEEK_FRAME, "rule": "one event-relative looping cursor starts at the nonzero seek and advances sample-exactly through all swaps"},
            "tail": {"passed": tail_complete, "manager_tail_retiring": manager.telemetry().tail_retiring.is_some(), "observed_tail_blocks": streamed_stats.tail_retiring_blocks, "rule": "retiring environmental tail is rendered to terminal completion before another world is admitted"},
            "no_restart": {"passed": streamed_stats.timeline_restarts == 0, "restart_count": streamed_stats.timeline_restarts, "rule": "the event-relative host cursor is never reset at a cell swap; oracle comparison independently gates rendered continuity"},
            "third_world_refusal": {"passed": third_world_refusal_passed, "expected": 3, "observed": streamed_stats.third_world_refusals, "rule": "CellStreamManager refuses a next candidate while the previous generation tail retires"},
        },
        "unchanged_level_gate": {"limit_db": LEVEL_GATE_DB, "max_abs_delta_db": gate.max_abs_delta_db, "max_delta_frame": gate.max_delta_frame, "oracle_rms_at_max": gate.oracle_rms_at_max, "streamed_rms_at_max": gate.streamed_rms_at_max, "samples_compared": gate.samples_compared, "expected_samples": gate.expected_samples, "passed": gate.passed, "rule": "oracle and streamed active-plane energy RMS at exact absolute callback frames"},
        "claims": ["actual four package/bake linked desktop callback evaluation", "oracle pass completed before streamed pass", "three public neutral world swaps with bounded prewarm/crossfade/tail retirement", "desktop host callback timing and process peak RSS observation"],
        "non_claims": ["desktop linked callback path only", "not hardware, iOS, AirPods, device, or listening evidence", "host timing and process peak RSS are not device thermal or device resource evidence", "host event cursor is not macro transaction, token, ACK, or lifecycle evidence", "logical frame cadence is not wall-clock device timing", "no acoustic approval beyond the stated one-decibel unchanged gate"],
    });
    let dir = AtomicDir::create(output.clone())?;
    atomicio::write_json_atomic(&dir.temp_path().join("report.json"), &report)?;
    dir.commit()?;
    let report_path = output.join("report.json");
    let report_hash = sha256_hex(
        &std::fs::read(&report_path)
            .map_err(|e| CliError::new(format!("cannot read report: {e}")))?,
    );
    println!("{}", serde_json::to_string(&json!({"report": path_string(&report_path), "sha256": report_hash, "status": report["status"]})).expect("summary JSON is finite"));
    if report["status"] != "passed" {
        return Err(CliError::new(format!(
            "neutral-swap-traverse gate failed; report {}",
            report_path.display()
        )));
    }
    Ok(())
}

struct LevelGate {
    passed: bool,
    max_abs_delta_db: Option<f64>,
    max_delta_frame: Option<u64>,
    oracle_rms_at_max: Option<f64>,
    streamed_rms_at_max: Option<f64>,
    samples_compared: usize,
    expected_samples: usize,
}

fn compare_levels(
    oracle: &[(u64, f64)],
    streamed: &[(u64, f64)],
    timeline_frames: u64,
) -> LevelGate {
    let mut stream_by_frame = std::collections::BTreeMap::new();
    for &(frame, level) in streamed {
        if frame < timeline_frames {
            stream_by_frame.insert(frame, level);
        }
    }
    let mut maximum = 0.0_f64;
    let mut maximum_detail = None;
    let mut count = 0;
    for &(frame, oracle_rms) in oracle {
        if frame >= timeline_frames {
            continue;
        }
        if let Some(stream_rms) = stream_by_frame.get(&frame) {
            let a = dbfs(oracle_rms);
            let b = dbfs(*stream_rms);
            if a.is_finite() && b.is_finite() {
                let delta = (a - b).abs();
                if delta > maximum {
                    maximum = delta;
                    maximum_detail = Some((frame, oracle_rms, *stream_rms));
                }
                count += 1;
            }
        }
    }
    let expected_samples = timeline_frames.div_ceil(BLOCK_SIZE as u64) as usize;
    LevelGate {
        passed: count == expected_samples && maximum <= LEVEL_GATE_DB,
        max_abs_delta_db: (count > 0).then_some(maximum),
        max_delta_frame: maximum_detail.map(|detail| detail.0),
        oracle_rms_at_max: maximum_detail.map(|detail| detail.1),
        streamed_rms_at_max: maximum_detail.map(|detail| detail.2),
        samples_compared: count,
        expected_samples,
    }
}

fn dbfs(rms: f64) -> f64 {
    if rms <= 0.0 {
        -300.0
    } else {
        20.0 * rms.log10()
    }
}

fn require_linked() -> Result<()> {
    if fightbox_steam_audio::backend_availability()
        .to_json()
        .contains(r#""status":"available""#)
    {
        Ok(())
    } else {
        Err(CliError::new(
            "phase-b neutral-swap-traverse requires --features linked-sdk and STEAM_AUDIO_SDK_DIR",
        ))
    }
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf> {
    let path = path.canonicalize().map_err(|e| {
        CliError::new(format!(
            "cannot canonicalize {label} {}: {e}",
            path.display()
        ))
    })?;
    if !path.is_file() {
        return Err(CliError::new(format!(
            "{label} {} is not a file",
            path.display()
        )));
    }
    Ok(path)
}

fn canonical_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let path = path.canonicalize().map_err(|e| {
        CliError::new(format!(
            "cannot canonicalize {label} {}: {e}",
            path.display()
        ))
    })?;
    if !path.is_dir() {
        return Err(CliError::new(format!(
            "{label} {} is not a directory",
            path.display()
        )));
    }
    Ok(path)
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

fn cell_short(record: &CityRouteCellRecord) -> String {
    format!("e{}-n{}", record.grid_index.east, record.grid_index.north)
}

fn load_four_cells(route: &CityRouteManifest, root: &Path) -> Result<Vec<CellArtifact>> {
    route
        .cells
        .iter()
        .map(|record| {
            let short = cell_short(record);
            let package = root.join("packages").join(format!("{short}.fightbox"));
            let bake = root.join("bakes").join(format!("{short}.baked"));
            let input = city_route::load_cell(&package, Some(&bake), false)?;
            let completed = input.completed_bake.as_ref();
            let record_completed = record.city_bake.completed.as_ref();
            if input.city_id != record.city_id
                || input.cell_id != record.cell_id
                || input.grid_index != record.grid_index
                || input.local_to_city_enu_m != record.local_to_city_enu_m
                || input.world_manifest_sha256 != record.world.manifest_sha256
                || input.mesh_sha256 != record.world.mesh_sha256
                || input.materials_sha256 != record.world.materials_sha256
                || input.probe_plan_sidecar_sha256 != record.city_bake.probe_plan_sidecar_sha256
                || input.completed_bake_sidecar_sha256.as_deref()
                    != record_completed.map(|bake| bake.sidecar_sha256.as_str())
                || completed.map(|bake| bake.probe_batch.serialized_sha256.as_str())
                    != record_completed.map(|bake| bake.probe_batch_sha256.as_str())
                || completed.map(|bake| bake.probe_batch.serialized_size_bytes)
                    != record_completed.map(|bake| bake.probe_batch_size_bytes)
                || input.probe_byte_estimate_v2_sha256.as_deref()
                    != record_completed
                        .and_then(|bake| bake.probe_byte_estimate_v2_sha256.as_deref())
                || input.probe_byte_estimate_v2_size_bytes
                    != record_completed.and_then(|bake| bake.probe_byte_estimate_v2_size_bytes)
                || input.installed_package_bytes != record.installed_size.package_bytes
                || input.installed_bake_bytes != record.installed_size.baked_artifact_bytes
            {
                return Err(CliError::new(format!(
                    "loaded package/bake identity differs from route cell {}",
                    record.cell_id
                )));
            }
            let loaded = read_package_with_capabilities(&package, &[CITY_BAKE_V2_CAPABILITY])
                .map_err(|e| {
                    CliError::new(format!("cannot load package {}: {e}", package.display()))
                })?;
            let mesh = city::scene_mesh(&loaded)?;
            let baked = city::load_baked(&bake)?;
            Ok(CellArtifact {
                record: record.clone(),
                package,
                bake,
                mesh,
                baked,
            })
        })
        .collect()
}

fn load_source_signal() -> Result<(Vec<f32>, SourceProfile)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/assets")
        .join(format!("{SOURCE_ASSET_ID}.json"));
    let bytes = std::fs::read(&path).map_err(|e| {
        CliError::new(format!(
            "cannot read source descriptor {}: {e}",
            path.display()
        ))
    })?;
    let descriptor = AssetDescriptor::parse(
        std::str::from_utf8(&bytes)
            .map_err(|e| CliError::new(format!("source descriptor is not UTF-8: {e}")))?,
    )
    .map_err(CliError::new)?;
    let resolved = descriptor.resolve().map_err(CliError::new)?;
    let (signal, analysis) = resolved.regenerate_mono().map_err(CliError::new)?;
    let profile = SourceProfile {
        id: SourceId::new("neutral-swap-source"),
        pose: Pose {
            position: EnuVector3::new(
                SOURCE_CITY[0] as f32,
                SOURCE_CITY[1] as f32,
                SOURCE_CITY[2] as f32,
            ),
            forward: EnuVector3::new(0.0, 1.0, 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
        },
        reference_level: ReferenceLevel::SplAtOneMeter {
            db_spl: SOURCE_DB_SPL,
        },
        asset_analysis: analysis.analysis().clone(),
        extent: ExtentDescriptor::Point,
        directivity: Directivity::default(),
        max_speed_mps: 0.0,
    };
    Ok((signal.samples, profile))
}

fn audio_config() -> AudioConfig {
    AudioConfig {
        sample_rate_hz: SAMPLE_RATE as i32,
        frame_size: BLOCK_SIZE as i32,
    }
}

fn descriptor_for_offset(offset: [f64; 3]) -> MultiSourceDescriptor {
    MultiSourceDescriptor::at(local_source(offset))
        .with_metadata_city_offset(EnuVector3::new(
            offset[0] as f32,
            offset[1] as f32,
            offset[2] as f32,
        ))
        .with_reference_level(ReferenceLevel::SplAtOneMeter {
            db_spl: SOURCE_DB_SPL,
        })
}

fn simulation_config() -> S3SimulationConfig {
    S3SimulationConfig {
        max_occlusion_samples: 64,
        direct_occlusion: DirectOcclusionMode::Raycast,
        reflection_rays: 4_096,
        reflection_bounces: 2,
        reflection_duration_s: 1.0,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        pathing_order: 2,
        validate_paths: true,
        find_alternate_paths: true,
        trace_path_validation: false,
        ..S3SimulationConfig::default()
    }
}

fn route_timeline_frames() -> u64 {
    let metres: f64 = WAYPOINTS
        .windows(2)
        .map(|pair| {
            let dx = pair[1][0] - pair[0][0];
            let dy = pair[1][1] - pair[0][1];
            (dx * dx + dy * dy).sqrt()
        })
        .sum();
    (metres / SPEED_MPS * f64::from(SAMPLE_RATE)).ceil() as u64
}

fn source_timeline_json(source: &(Vec<f32>, SourceProfile)) -> Value {
    json!({
        "asset_id": SOURCE_ASSET_ID,
        "sample_rate_hz": SAMPLE_RATE,
        "signal_frames": source.0.len(),
        "mapping": "source_frame = (absolute_frame - emission_frame + program_seek_frame) mod signal_frames; one cursor survives every cell swap",
        "event": {"event_id": EVENT_ID, "emission_frame": EVENT_EMISSION_FRAME, "program_seek_frame": EVENT_SEEK_FRAME, "absolute": true, "looping": true, "authority": "desktop harness host feed; not macro transaction or ACK evidence"},
    })
}

fn render_oracle(
    package: &Path,
    bake: &Path,
    source: &(Vec<f32>, SourceProfile),
    config: S3SimulationConfig,
    timeline_frames: u64,
) -> Result<(Vec<(u64, f64)>, CallbackStats)> {
    let loaded = read_package_with_capabilities(package, &[])
        .map_err(|e| CliError::new(format!("cannot load oracle package: {e}")))?;
    let mesh = city::scene_mesh(&loaded)?;
    let baked = city::load_baked(bake)?;
    let descriptor = MultiSourceDescriptor::at(EnuVector3::new(
        SOURCE_CITY[0] as f32,
        SOURCE_CITY[1] as f32,
        SOURCE_CITY[2] as f32,
    ))
    .with_reference_level(ReferenceLevel::SplAtOneMeter {
        db_spl: SOURCE_DB_SPL,
    });
    let (simulation, render) = build_spatial_multi_source_session(
        &mesh,
        &baked,
        AudioConfig {
            sample_rate_hz: SAMPLE_RATE as i32,
            frame_size: BLOCK_SIZE as i32,
        },
        config,
        &[descriptor],
        &[1],
        2,
    )
    .map_err(backend_error)?;
    let mut session = make_session(simulation, render, source.1.clone(), [0.0, 0.0, 0.0], 0)?;
    let mut stats = CallbackStats::default();
    render_blocks(
        &mut session,
        &source.0,
        timeline_frames,
        timeline_frames,
        None,
        &mut stats,
    )?;
    let levels = stats.levels_by_frame.clone();
    drop(session);
    Ok((levels, stats))
}

fn make_session(
    simulation: SteamAudioSpatialSimulationRunner,
    mut render: SteamAudioSpatialRenderGraph,
    _profile: SourceProfile,
    active_offset: [f64; 3],
    initial_frame: u64,
) -> Result<SessionParts> {
    let listener = listener_state_local(route_position(initial_frame), active_offset);
    let update = simulation_update(listener, local_source(active_offset));
    let mut simulation = simulation;
    simulation
        .prepare_simulation_for_realtime(&update)
        .map_err(|error| {
            CliError::new(format!(
                "initial neutral simulation preparation failed: {error:?}"
            ))
        })?;
    render.prepare_for_realtime().map_err(|error| {
        CliError::new(format!(
            "initial neutral callback preparation failed: {error:?}"
        ))
    })?;
    Ok(SessionParts {
        simulation,
        graph: render,
        source_signal: Vec::new(),
    })
}

fn render_blocks(
    session: &mut SessionParts,
    signal: &[f32],
    callback_frames: u64,
    source_timeline_frames: u64,
    mut route: Option<(&[CellArtifact], &mut CellStreamManager<()>)>,
    stats: &mut CallbackStats,
) -> Result<()> {
    if session.source_signal.is_empty() {
        session.source_signal = signal.to_vec();
    }
    let mut active_offset = route
        .as_ref()
        .map_or([0.0; 3], |(cells, _)| cells[0].record.local_to_city_enu_m);
    let mut active_cell = route
        .as_ref()
        .map(|(cells, _)| cells[0].record.cell_id.clone());
    let mut swap_index = 0usize;
    let mut event_program_cursor: Option<u64> = None;
    let mut next_control_frame = 0_u64;
    let mut control_index = 0_u64;
    let mut control_listener_global = route_position(0);
    let mut active_transition: Option<(u64, u64)> = None;
    // The callback banks and metadata are caller-owned scratch: the backend
    // zero-fills both banks and rewrites every metadata field on each block,
    // so they are allocated once here instead of once per block.
    let mut presentation = vec![0.0; 48 * BLOCK_SIZE];
    let mut environmental = vec![0.0; 9 * BLOCK_SIZE];
    let mut metadata = SpatialOutputMetadata::default();
    for block in 0..(callback_frames as usize).div_ceil(BLOCK_SIZE) {
        let frame = (block * BLOCK_SIZE) as u64;
        let global = route_position(frame);
        if let Some((cells, manager)) = route.as_mut() {
            let owner = owner_for_position(cells, global).ok_or_else(|| {
                CliError::new(format!(
                    "route has no owner at [{:.3},{:.3}]",
                    global[0], global[1]
                ))
            })?;
            // A route boundary may arrive while the prior environmental tail
            // is still retiring. Keep rendering the old owner until the public
            // session reports collection; never ask the neutral swap layer to
            // admit a third generation.
            if manager.telemetry().tail_retiring.is_some() {
                let session_state = session.simulation.cell_stream_state();
                let session_is_terminal = matches!(
                    session_state,
                    fightbox_steam_audio::SpatialCellStreamState::Idle
                        | fightbox_steam_audio::SpatialCellStreamState::TailComplete
                );
                if session_is_terminal || session.simulation.collect_retired_world() {
                    let _ = session.simulation.collect_retired_world();
                    let _ = manager.finish_tail_retirement();
                }
            }
            if active_cell.as_deref() != Some(owner.record.cell_id.as_str())
                && manager.telemetry().tail_retiring.is_none()
            {
                let target_index = cells
                    .iter()
                    .position(|cell| cell.record.cell_id == owner.record.cell_id)
                    .unwrap();
                if target_index != swap_index + 1 {
                    return Err(CliError::new(
                        "route owner changed without exactly one sequential swap",
                    ));
                }
                let adopted_generation = perform_swap(
                    &mut session.simulation,
                    manager,
                    &cells[swap_index],
                    owner,
                    target_index,
                    &mut active_offset,
                    frame,
                )?;
                active_transition = Some((adopted_generation, 0));
                active_cell = Some(owner.record.cell_id.clone());
                swap_index = target_index;
                let refusal_target = cells
                    .get(target_index + 1)
                    .or_else(|| cells.get(target_index.saturating_sub(1)))
                    .unwrap();
                let refusal = manager.request_prepare(
                    cell_identity(refusal_target),
                    estimate_for(refusal_target),
                    fresh_memory(),
                );
                if !matches!(
                    refusal,
                    PrepareAdmission::Refused(fightbox_runtime::PrepareRefusalReason::TailRetiring)
                ) {
                    return Err(CliError::new(
                        "intentional third-world request was not refused while old tail retired",
                    ));
                }
                stats.third_world_refusals += 1;
            }
            if session.simulation.collect_retired_world() {
                let _ = manager.finish_tail_retirement();
            }
        }
        // Drive the control lane at exactly 60 Hz. Four controls land in
        // every 15 Hz route interval. During a handoff the production runner
        // translates this one city-space state into both retained cell-local
        // simulations and advances their correlation sequence together.
        while next_control_frame <= frame {
            control_listener_global = route_position(next_control_frame);
            let control_listener = listener_state_local(control_listener_global, active_offset);
            let control_source = local_source(active_offset);
            let update = simulation_update(control_listener, control_source);
            session.simulation.update_inputs(&update);
            session.simulation.run_direct().map_err(|error| {
                CliError::new(format!(
                    "neutral direct control failed at frame {next_control_frame}: {error:?}"
                ))
            })?;
            if control_index.is_multiple_of(CONTROLS_PER_ROUTE_SAMPLE) {
                session.simulation.run_pathing().map_err(|error| {
                    CliError::new(format!(
                        "neutral pathing control failed at frame {next_control_frame}: {error:?}"
                    ))
                })?;
                stats.pathing_updates += 1;
            }
            if control_index.is_multiple_of(CONTROL_HZ / 5) {
                session.simulation.run_reflections().map_err(|error| {
                    CliError::new(format!(
                        "neutral reflection control failed at frame {next_control_frame}: {error:?}"
                    ))
                })?;
                stats.reflection_updates += 1;
            }
            stats.control_updates += 1;
            if next_control_frame < source_timeline_frames {
                stats.route_control_updates += 1;
            }
            control_index += 1;
            next_control_frame += CONTROL_INTERVAL_FRAMES;
        }
        let listener = listener_state_local(control_listener_global, active_offset);
        let source_local = local_source(active_offset);
        let propagation_sequence = session.simulation.latest_direct_sequence();
        let signal_frames = session.source_signal.len() as u64;
        let mut source_plane = [0.0_f32; BLOCK_SIZE];
        for (index, sample) in source_plane.iter_mut().enumerate() {
            let absolute = frame + index as u64;
            if absolute < EVENT_EMISSION_FRAME || absolute >= source_timeline_frames {
                *sample = 0.0;
                continue;
            }
            let closed_form = (absolute - EVENT_EMISSION_FRAME + EVENT_SEEK_FRAME) % signal_frames;
            let cursor = event_program_cursor.get_or_insert(EVENT_SEEK_FRAME % signal_frames);
            stats.event_checks += 1;
            if *cursor != closed_form {
                stats.event_failures += 1;
                stats.timeline_restarts += 1;
            }
            *sample = session.source_signal[*cursor as usize];
            *cursor = (*cursor + 1) % signal_frames;
        }
        let sources = [SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&source_plane, &[]],
        }];
        if session.simulation.cell_stream_state()
            == fightbox_steam_audio::SpatialCellStreamState::TailRetiring
        {
            stats.tail_retiring_blocks += 1;
        }
        let started = Instant::now();
        let result = session
            .graph
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame: frame,
                propagation_sequence,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environmental,
                metadata: &mut metadata,
            });
        stats
            .callback_ns
            .push(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        stats.callbacks += 1;
        if let Err(error) = result {
            stats.backend_failures += 1;
            return Err(CliError::new(format!(
                "neutral callback failed at frame {frame}: {error:?}"
            )));
        }
        if metadata.validity != SpatialOutputValidity::Valid {
            stats.silent_blocks += 1;
        }
        if let Some((adopted_generation, transition_block)) = active_transition.as_mut() {
            if *transition_block < EXPECTED_WARMUP_BLOCKS {
                stats.warmup_blocks += 1;
                if metadata.generation == *adopted_generation {
                    stats.transition_metadata_failures += 1;
                }
            } else if *transition_block < EXPECTED_WARMUP_BLOCKS + EXPECTED_CROSSFADE_BLOCKS {
                stats.crossfade_blocks += 1;
                if metadata.generation != *adopted_generation {
                    stats.transition_metadata_failures += 1;
                }
            }
            *transition_block += 1;
            if *transition_block >= EXPECTED_WARMUP_BLOCKS + EXPECTED_CROSSFADE_BLOCKS {
                active_transition = None;
            }
        }
        stats.metadata_generations.push(metadata.generation);
        stats.validities.push(match metadata.validity {
            SpatialOutputValidity::Valid => "Valid",
            SpatialOutputValidity::Invalid => "Invalid",
            SpatialOutputValidity::SilentDiscontinuity => "SilentDiscontinuity",
        });
        let feed = metadata.presentation_feeds[0];
        stats.direction_checks += 1;
        let expected_direction = direction(source_local, listener.pose.position);
        let direction_error = squared_distance(feed.direction_enu, expected_direction).sqrt();
        if !feed.valid || direction_error > 0.02 {
            stats.direction_failures += 1;
        }
        let output_energy_rms = spatial_output_energy_rms(
            &presentation,
            metadata.active_presentation_feed_count,
            &environmental,
            metadata.active_environmental_plane_count,
            BLOCK_SIZE,
        );
        stats.levels_by_frame.push((frame, output_energy_rms));
    }
    if let Some((_, manager)) = route {
        eprintln!(
            "neutral traverse final lifecycle={:?} manager={:?}",
            session.simulation.cell_stream_state(),
            manager.telemetry()
        );
        if manager.telemetry().tail_retiring.is_some() {
            return Err(CliError::new(
                "route ended before final environmental tail retirement",
            ));
        }
    }
    Ok(())
}

fn perform_swap(
    simulation: &mut SteamAudioSpatialSimulationRunner,
    manager: &mut CellStreamManager<()>,
    _active: &CellArtifact,
    target: &CellArtifact,
    target_index: usize,
    active_offset: &mut [f64; 3],
    frame: u64,
) -> Result<u64> {
    let identity = cell_identity(target);
    let estimate = estimate_for(target);
    let ticket = match manager.request_prepare(identity, estimate, fresh_memory()) {
        PrepareAdmission::Queued(ticket) => ticket,
        other => {
            return Err(CliError::new(format!(
                "cell {} was not admitted for swap {target_index}: {other:?}",
                target.record.cell_id
            )));
        }
    };
    let job = manager
        .start_prepare(
            ticket,
            std::time::Duration::from_nanos(
                frame.saturating_mul(1_000_000_000) / u64::from(SAMPLE_RATE),
            ),
        )
        .map_err(|e| CliError::new(format!("start prepare failed: {e}")))?;
    let target_offset = target.record.local_to_city_enu_m;
    let mut prepared = simulation
        .prepare_world_with_metadata_city_offset(
            &target.mesh,
            &target.baked,
            EnuVector3::new(
                target_offset[0] as f32,
                target_offset[1] as f32,
                target_offset[2] as f32,
            ),
        )
        .map_err(backend_error)?;
    let target_update = simulation_update(
        listener_state_local(route_position(frame), target_offset),
        local_source(target_offset),
    );
    prepared
        .prepare_simulation_for_realtime(&target_update)
        .map_err(|e| CliError::new(format!("candidate simulation preparation failed: {e:?}")))?;
    let actual = target.record.installed_size.actual_installed_bytes;
    if manager
        .complete_prepare(
            job.ticket,
            (),
            actual,
            std::time::Duration::from_nanos(
                frame.saturating_mul(1_000_000_000) / u64::from(SAMPLE_RATE) + 1,
            ),
        )
        .map_err(|e| CliError::new(format!("complete prepare failed: {e}")))?
        != CompletePreparation::Prepared
    {
        return Err(CliError::new(
            "cell preparation was rejected after linked world construction",
        ));
    }
    let receipt = simulation
        .swap_prepared_world(prepared)
        .map_err(|e| CliError::new(format!("neutral world swap {target_index} failed: {e:?}")))?;
    manager
        .adopt_prepared(ticket)
        .map_err(|e| CliError::new(format!("cell manager adoption failed: {e}")))?;
    *active_offset = target.record.local_to_city_enu_m;
    Ok(receipt.generation)
}

fn cell_identity(cell: &CellArtifact) -> CellIdentity {
    CellIdentity::new(cell.record.city_id.clone(), cell.record.cell_id.clone())
}
fn estimate_for(cell: &CellArtifact) -> CellPrepareEstimate {
    CellPrepareEstimate {
        raw_cell_bytes: cell.record.prefetch.raw_cell_bytes,
        prepared_resident_bytes: cell.record.prefetch.prepared_resident_estimate_bytes,
        preparation_scratch_bytes: cell.record.prefetch.preparation_scratch_bytes,
    }
}
fn fresh_memory() -> FreshMemorySample {
    FreshMemorySample {
        advisory_reserve_bytes: 900 * fightbox_runtime::MIB,
        process_resident_bytes: 210 * fightbox_runtime::MIB,
    }
}
fn local_source(offset: [f64; 3]) -> EnuVector3 {
    EnuVector3::new(
        (SOURCE_CITY[0] - offset[0]) as f32,
        (SOURCE_CITY[1] - offset[1]) as f32,
        (SOURCE_CITY[2] - offset[2]) as f32,
    )
}
fn listener_state_local(global: [f64; 3], offset: [f64; 3]) -> ListenerState {
    ListenerState {
        pose: Pose {
            position: EnuVector3::new(
                (global[0] - offset[0]) as f32,
                (global[1] - offset[1]) as f32,
                (global[2] - offset[2]) as f32,
            ),
            forward: EnuVector3::new(0.0, 1.0, 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
        },
        linear_velocity_mps: EnuVector3::default(),
    }
}
fn simulation_update(listener: ListenerState, source: EnuVector3) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
        pose: Pose {
            position: source,
            forward: EnuVector3::new(0.0, 1.0, 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
        },
        linear_velocity_mps: EnuVector3::default(),
    };
    SimulationUpdate { listener, sources }
}
fn direction(source: EnuVector3, listener: EnuVector3) -> EnuVector3 {
    let d = EnuVector3::new(
        source.east_m - listener.east_m,
        source.north_m - listener.north_m,
        source.up_m - listener.up_m,
    );
    let n = (d.east_m * d.east_m + d.north_m * d.north_m + d.up_m * d.up_m).sqrt();
    if n <= 1e-6 {
        EnuVector3::new(0.0, 1.0, 0.0)
    } else {
        EnuVector3::new(d.east_m / n, d.north_m / n, d.up_m / n)
    }
}
fn squared_distance(a: EnuVector3, b: EnuVector3) -> f32 {
    let d = EnuVector3::new(a.east_m - b.east_m, a.north_m - b.north_m, a.up_m - b.up_m);
    d.east_m * d.east_m + d.north_m * d.north_m + d.up_m * d.up_m
}
fn spatial_output_energy_rms(
    presentation: &[f32],
    presentation_planes: usize,
    environmental: &[f32],
    environmental_planes: usize,
    frames: usize,
) -> f64 {
    if frames == 0 {
        return 0.0;
    }
    let presentation_samples = presentation_planes
        .saturating_mul(frames)
        .min(presentation.len());
    let environmental_samples = environmental_planes
        .saturating_mul(frames)
        .min(environmental.len());
    let sum = presentation[..presentation_samples]
        .iter()
        .chain(environmental[..environmental_samples].iter())
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>();
    (sum / frames as f64).sqrt()
}

fn route_position(frame: u64) -> [f64; 3] {
    let mut remaining = frame as f64 / f64::from(SAMPLE_RATE) * SPEED_MPS;
    for pair in WAYPOINTS.windows(2) {
        let dx = pair[1][0] - pair[0][0];
        let dy = pair[1][1] - pair[0][1];
        let dz = pair[1][2] - pair[0][2];
        let distance = (dx * dx + dy * dy + dz * dz).sqrt();
        if remaining <= distance {
            let f = if distance == 0.0 {
                0.0
            } else {
                remaining / distance
            };
            return [
                pair[0][0] + dx * f,
                pair[0][1] + dy * f,
                pair[0][2] + dz * f,
            ];
        }
        remaining -= distance;
    }
    WAYPOINTS[WAYPOINTS.len() - 1]
}
fn owner_for_position<'a>(cells: &'a [CellArtifact], pos: [f64; 3]) -> Option<&'a CellArtifact> {
    let e = (pos[0] * 1000.0).round() as i64;
    let n = (pos[1] * 1000.0).round() as i64;
    cells.iter().find(|c| {
        let b = c.record.selection.ownership_bounds_city_enu_mm;
        b.min[0] <= e && e < b.max[0] && b.min[1] <= n && n < b.max[1]
    })
}
fn backend_error(error: BackendError) -> CliError {
    CliError::new(format!("linked Steam Audio error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::callback_resource_observation;

    #[test]
    fn callback_resource_observation_rejects_empty_input() {
        assert!(callback_resource_observation(&[]).is_err());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn callback_resource_observation_reports_timing_and_peak_rss() {
        let observation = callback_resource_observation(&[1_000, 2_000, 3_000])
            .expect("supported desktop process resource observation");
        assert_eq!(observation.sample_count, 3);
        assert!(observation.p50_ms >= 0.002);
        assert!(observation.p99_ms >= 0.003);
        assert!(observation.p99_9_ms >= observation.p99_ms);
        assert_eq!(observation.max_ms, 0.003);
        assert!(observation.peak_rss_mib > 0.0);
    }
}
