//! Phase A retained-stage and kilometer-scale sweep.
//!
//! Every bake, validation load, and retained benchmark runs serially in an
//! isolated process group. These are offline per-source stage timings, not
//! audio-callback measurements.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, BakedProbeBatch, DirectOcclusionMode, EnuVector3, ListenerPose,
    PROBE_BATCH_METADATA_SCHEMA, PathBakeConfig, ProbeBatchMetadata, ProbeVolume,
    ReflectionEffectConfig, ReflectionEffectType, S3BakeRequest, S3BenchmarkIterations,
    S3BenchmarkOutput, S3BenchmarkRequest, S3RenderRequest, S3SimulationConfig,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SceneMesh, bake_s3, benchmark_s3_stages,
    render_s3, sha256_hex,
};
use serde::{Deserialize, Serialize};

use crate::atomicio::{AtomicDir, validate_output_path, write_bytes_atomic, write_json_atomic};
use crate::error::{CliError, Result};
use crate::provenance::{self, SdkBinary};

const SCHEMA: &str = "fightbox.phase-a-sweep.v1";
const ARTIFACT_SCHEMA: &str = "fightbox.phase-a-sweep-artifacts.v1";
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const POSITIVE_BAKE_LIMIT_MS: u64 = 10 * 60 * 1000;
const CHILD_TIMEOUT_MS: u64 = 15 * 60 * 1000;
const MAX_RSS_BUDGET: u64 = 4 * GIB;
const LIVE_RSS_KILL: u64 = 8 * GIB;
const PATH_DATA_BUDGET: u64 = 256 * MIB;
const PACKAGE_BUDGET: u64 = 512 * MIB;
const TEMP_CAP: u64 = GIB;
const PROBE_CAP: u64 = 4_096;
const PAIR_CAP: u64 = 16_777_216;
const ANALYTIC_AZIMUTH_DEGREES: f64 = 303.690_068;
const ANALYTIC_AZIMUTH_TOLERANCE_DEGREES: f64 = 5.0;
const PINNED_LIBPHONON_SHA256: &str =
    "7cedf26c7e1fdb378971989236c52af6723246226381f14da855a028f142f92c";
const BUNDLED_LIBPHONON_PATH: &str = "authority/libphonon.dylib";
const BUNDLED_ENGINE_PATH: &str = "authority/fightbox";
const DECISION_REQUIRED_BY: &str = "2026-08-05";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepMode {
    Sampled,
    Full,
}

pub enum SweepCommand {
    Execute { out: PathBuf, mode: SweepMode },
    Verify { report: PathBuf },
}

pub fn parse_args(args: &[String]) -> Result<SweepCommand> {
    let mut out = None;
    let mut verify = None;
    let mut mode = SweepMode::Sampled;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--out" => out = Some(PathBuf::from(next(&mut iter, "--out")?)),
            "--verify" => verify = Some(PathBuf::from(next(&mut iter, "--verify")?)),
            "--mode" => {
                mode = match next(&mut iter, "--mode")? {
                    "sampled" => SweepMode::Sampled,
                    "full" => SweepMode::Full,
                    value => {
                        return Err(CliError::new(format!(
                            "invalid sweep mode {value:?}; expected sampled or full"
                        )));
                    }
                }
            }
            value => {
                return Err(CliError::new(format!(
                    "unknown sweep argument {value:?}; expected --out, --mode sampled|full, or --verify"
                )));
            }
        }
    }
    match (out, verify) {
        (Some(out), None) => Ok(SweepCommand::Execute { out, mode }),
        (None, Some(report)) if mode == SweepMode::Sampled => Ok(SweepCommand::Verify { report }),
        (None, None) => Err(CliError::new(
            "phase-a sweep requires --out <report-directory> (or --verify <report-directory>)",
        )),
        _ => Err(CliError::new(
            "--out/--mode and --verify are mutually exclusive",
        )),
    }
}

fn next<'a>(iter: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<&'a str> {
    iter.next()
        .map(String::as_str)
        .ok_or_else(|| CliError::new(format!("{flag} requires a value")))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Protocol {
    standard_warmups: u32,
    standard_measured: u32,
    reflection_warmups: u32,
    reflection_measured: u32,
    effect_warmups: u32,
    effect_measured: u32,
    local_bakes: usize,
    path_order_cases: usize,
    canonical_cross_cases: usize,
    interaction_guards: usize,
    kilometer_full_cells: usize,
}

impl Protocol {
    fn for_mode(mode: SweepMode) -> Self {
        let counts = match mode {
            SweepMode::Sampled => (1, 4, 1, 2, 1, 4),
            SweepMode::Full => (64, 1_024, 32, 256, 64, 1_024),
        };
        Self {
            standard_warmups: counts.0,
            standard_measured: counts.1,
            reflection_warmups: counts.2,
            reflection_measured: counts.3,
            effect_warmups: counts.4,
            effect_measured: counts.5,
            local_bakes: 12,
            path_order_cases: 48,
            canonical_cross_cases: 36,
            interaction_guards: 8,
            kilometer_full_cells: 6,
        }
    }

    fn iterations(&self) -> S3BenchmarkIterations {
        S3BenchmarkIterations {
            simulation_warmup: self.standard_warmups,
            simulation_measured: self.standard_measured,
            reflection_warmup: self.reflection_warmups,
            reflection_measured: self.reflection_measured,
            effect_warmup: self.effect_warmups,
            effect_measured: self.effect_measured,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CaseStatus {
    Passed,
    NoPath,
    QualityFailed,
    ProjectedSkip,
    ResourceKilled,
    Timeout,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resources {
    wall_time_ms: Option<u64>,
    child_exit_code: Option<i32>,
    child_signal: Option<i32>,
    time_l_max_rss_bytes: Option<u64>,
    sampled_rss_bytes: Vec<u64>,
    peak_sampled_rss_bytes: Option<u64>,
    temp_directory_peak_bytes: Option<u64>,
    termination: Option<String>,
}

impl Resources {
    fn unrun() -> Self {
        Self {
            wall_time_ms: None,
            child_exit_code: None,
            child_signal: None,
            time_l_max_rss_bytes: None,
            sampled_rss_bytes: Vec::new(),
            peak_sampled_rss_bytes: None,
            temp_directory_peak_bytes: None,
            termination: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Percentiles {
    n: usize,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    max_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stage {
    raw_ns: Vec<u64>,
    derived: Percentiles,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FiniteEvidence {
    direct_simulation: bool,
    path_simulation: bool,
    reflection_simulation: bool,
    direct_effect_binaural_apply: bool,
    path_effect_apply: bool,
    reflection_effect_decode_apply: bool,
    direct_simulation_samples_checked: u32,
    path_simulation_samples_checked: u32,
    reflection_simulation_samples_checked: u32,
    direct_effect_samples_checked: u32,
    path_effect_samples_checked: u32,
    reflection_effect_samples_checked: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BenchmarkEvidence {
    loaded_probe_count: u32,
    loaded_path_data_size_bytes: u64,
    retained_rendered_blocks: u32,
    requested_settings: serde_json::Value,
    delivered_settings: serde_json::Value,
    direct_simulation: Stage,
    path_simulation: Stage,
    reflection_simulation: Stage,
    direct_effect_binaural_apply: Stage,
    path_effect_apply: Stage,
    reflection_effect_decode_apply: Stage,
    finite: FiniteEvidence,
    budget_result: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidationEvidence {
    fresh_process: bool,
    path_output_nonzero: bool,
    path_output_finite: bool,
    analytic_arrival_degrees: f64,
    delivered_arrival_degrees: Option<f64>,
    angular_error_degrees: Option<f64>,
    analytic_arrival_passed: bool,
    validation_segments: usize,
    occluded_validation_segments: usize,
    requested_settings: serde_json::Value,
    delivered_settings: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseReport {
    id: String,
    family: String,
    input_hash: String,
    configuration_hash: String,
    status: CaseStatus,
    reason: Option<String>,
    requested: serde_json::Value,
    bake_delivered: Option<serde_json::Value>,
    runtime_delivered: Option<serde_json::Value>,
    resources: Resources,
    validation_status: Option<CaseStatus>,
    validation_reason: Option<String>,
    validation_resources: Option<Resources>,
    probe_count: Option<u32>,
    path_data_bytes: Option<u64>,
    serialized_package_bytes: Option<u64>,
    package_sha256: Option<String>,
    benchmark: Option<BenchmarkEvidence>,
    validation: Option<ValidationEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    applicability: String,
    kernel_reach: Option<bool>,
    monolithic_economics: Option<bool>,
    decision: String,
    named_post_mvp_phase: Option<String>,
    decision_required_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    schema_version: String,
    mode: SweepMode,
    generated_unix_seconds: u64,
    authority: AuthorityProvenance,
    protocol: Protocol,
    cases: Vec<CaseReport>,
    decision: Decision,
    claims: Vec<String>,
    non_claims: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityProvenance {
    engine_identity: String,
    source_state: String,
    source_identity_sha256: String,
    engine_executable_path: String,
    engine_executable_sha256: String,
    build_profile: String,
    platform: String,
    cpu_class: String,
    steam_audio_version: String,
    steam_audio_upstream_commit: String,
    steam_audio_dylib_path: String,
    steam_audio_dylib_sha256: String,
    sample_rate_hz: i32,
    canonical_plan_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    relative_path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataWire {
    probe_count: u32,
    path_data_size_bytes: u64,
    serialized_size_bytes: u64,
    content_sha256: String,
    bake_progress_callback_count: u32,
    final_bake_progress_millionths: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BakeWire {
    metadata: MetadataWire,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildEnvelope {
    status: CaseStatus,
    reason: Option<String>,
    bake: Option<BakeWire>,
    benchmark: Option<BenchmarkEvidence>,
    validation: Option<ValidationEvidence>,
    delivered: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum World {
    Local,
    Kilometer,
}

#[derive(Clone, Debug)]
struct BakeSpec {
    id: String,
    world: World,
    spacing_m: f32,
    path_range_m: f32,
    predicted_probes: u64,
    predicted_pairs: u64,
    positive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectVariant {
    Raycast,
    Volumetric05,
    Volumetric10,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReflectionVariant {
    Convolution,
    Hybrid,
    Parametric,
}

#[derive(Clone, Debug)]
struct RuntimeSpec {
    id: String,
    bake_id: String,
    spacing_m: f32,
    path_range_m: f32,
    block_size: i32,
    path_order: i32,
    direct: DirectVariant,
    reflection: ReflectionVariant,
}

struct ExpectedCase {
    family: &'static str,
    input: serde_json::Value,
    requested: serde_json::Value,
    selected: bool,
}

pub fn run(command: SweepCommand) -> Result<()> {
    match command {
        SweepCommand::Execute { out, mode } => execute(&out, mode),
        SweepCommand::Verify { report } => {
            verify_report(&report)?;
            println!("verified {}", report.join("report.json").display());
            Ok(())
        }
    }
}

fn execute(out: &Path, mode: SweepMode) -> Result<()> {
    let sdk = SdkBinary::detect();
    sdk.require_established()?;
    let out = validate_output_path(out)?;
    let atomic = AtomicDir::create(out.clone())?;
    let root = atomic.temp_path().to_path_buf();
    fs::create_dir(root.join("cases")).map_err(io("create sweep cases directory"))?;
    let authority = stage_authority_artifacts(&root, mode, &Protocol::for_mode(mode), &sdk)?;
    let protocol = Protocol::for_mode(mode);
    let mut cases = Vec::new();
    let mut baked_dirs = BTreeMap::new();

    for spec in bake_plan() {
        let selected = bake_selected(mode, &spec);
        if !selected {
            let reason = if mode == SweepMode::Full && spec.spacing_m == 12.5 {
                "projection-only spacing 12.5m exceeds prelaunch containment caps"
            } else {
                "not selected by fast sampled mode"
            };
            cases.push(projected_bake(&spec, reason)?);
            continue;
        }
        if spec.predicted_probes > PROBE_CAP || spec.predicted_pairs > PAIR_CAP {
            cases.push(projected_bake(
                &spec,
                "predicted probe/pair containment cap exceeded",
            )?);
            continue;
        }
        let dir = root.join("cases").join(&spec.id);
        fs::create_dir(&dir).map_err(io("create bake case directory"))?;
        let mut case = execute_bake(&spec, &dir)?;
        if matches!(case.status, CaseStatus::Passed | CaseStatus::QualityFailed)
            && spec.world == World::Kilometer
            && case.package_sha256.is_some()
        {
            let economics_status = case.status;
            let economics_reason = case.reason.clone();
            let (validation_run, envelope) = execute_validation(&spec, &dir)?;
            let validation_status = classify(&validation_run, envelope.as_ref());
            let validation_reason = validation_run
                .reason
                .clone()
                .or_else(|| envelope.as_ref().and_then(|value| value.reason.clone()))
                .or_else(|| {
                    (validation_status != CaseStatus::Passed)
                        .then_some("validation child produced no usable success envelope".into())
                });
            case.validation_status = Some(validation_status);
            case.validation_reason = validation_reason.clone();
            case.validation_resources = Some(validation_run.resources);
            case.validation = envelope.as_ref().and_then(|value| value.validation.clone());
            if !spec.positive {
                case.status = if validation_status == CaseStatus::Passed {
                    CaseStatus::QualityFailed
                } else {
                    CaseStatus::NoPath
                };
                case.reason = Some(if validation_status == CaseStatus::Passed {
                    "designated 1000m negative unexpectedly produced a validated path".into()
                } else {
                    "designated 1000m below-route negative produced no validated path".into()
                });
            } else if validation_status != CaseStatus::Passed {
                case.status = validation_status;
                case.reason = validation_reason;
            } else if economics_status == CaseStatus::QualityFailed {
                case.status = economics_status;
                case.reason = economics_reason;
            }
        }
        if case.status == CaseStatus::Passed {
            baked_dirs.insert(spec.id.clone(), dir);
        }
        cases.push(case);
    }

    for spec in runtime_plan() {
        let selected = runtime_selected(mode, &spec);
        if !selected {
            cases.push(projected_runtime(
                &spec,
                &protocol,
                "not selected by fast sampled mode",
            )?);
            continue;
        }
        let Some(bake_dir) = baked_dirs.get(&spec.bake_id) else {
            cases.push(projected_runtime(
                &spec,
                &protocol,
                "required bake did not pass",
            )?);
            continue;
        };
        let dir = root.join("cases").join(&spec.id);
        fs::create_dir(&dir).map_err(io("create runtime case directory"))?;
        cases.push(execute_runtime(&spec, bake_dir, &dir, &protocol)?);
    }

    let report = Report {
        schema_version: SCHEMA.into(),
        mode,
        generated_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        authority,
        protocol,
        decision: formal_decision(mode, &cases, DECISION_REQUIRED_BY),
        cases,
        claims: vec![
            "Retained offline per-source Steam Audio stages are reported only for attempted runtime cases carrying raw benchmark evidence.".into(),
        ],
        non_claims: vec![
            "This report does not measure or claim audio-callback performance.".into(),
            "Projected, skipped, killed, timed-out, failed, and no-path cells are not passes.".into(),
            "Tiling does not prove cross-tile pathing: Steam Audio pathing consumes one probe batch.".into(),
            "This sweep does not by itself complete Phase A or the human listening gate.".into(),
            provenance::UNCOMMITTED_SOURCE_NONCLAIM.into(),
            "Semantic verification is build-coupled: the verifier executable must be byte-identical to the generating executable bundled in this report.".into(),
        ],
    };
    write_json_atomic(&root.join("report.json"), &report)?;
    write_json_atomic(&root.join("artifacts.json"), &build_manifest(&root)?)?;
    verify_staged(&root)?;
    atomic.commit()?;
    verify_report(&out)?;
    println!("wrote and verified {}", out.join("report.json").display());
    Ok(())
}

fn bake_plan() -> Vec<BakeSpec> {
    let mut plan = Vec::new();
    for spacing in [0.5_f32, 1.0, 2.0, 4.0] {
        for range in [25.0_f32, 100.0, 250.0] {
            plan.push(BakeSpec {
                id: format!("local-s{}-r{}", compact(spacing), compact(range)),
                world: World::Local,
                spacing_m: spacing,
                path_range_m: range,
                predicted_probes: 0,
                predicted_pairs: 0,
                positive: true,
            });
        }
    }
    for (spacing, probes) in [
        (100.0_f32, 144_u64),
        (50.0, 529),
        (25.0, 2_025),
        (12.5, 7_921),
    ] {
        for range in [1_000.0_f32, 1_750.0, 2_500.0] {
            plan.push(BakeSpec {
                id: format!("km-s{}-r{}", compact(spacing), compact(range)),
                world: World::Kilometer,
                spacing_m: spacing,
                path_range_m: range,
                predicted_probes: probes,
                predicted_pairs: probes * probes,
                positive: range > 1_000.0,
            });
        }
    }
    plan
}

fn runtime_plan() -> Vec<RuntimeSpec> {
    let mut plan = Vec::new();
    for spacing in [0.5_f32, 1.0, 2.0, 4.0] {
        for range in [25.0_f32, 100.0, 250.0] {
            for order in 0..=3 {
                plan.push(runtime_spec(
                    format!(
                        "runtime-order-s{}-r{}-o{order}",
                        compact(spacing),
                        compact(range)
                    ),
                    format!("local-s{}-r{}", compact(spacing), compact(range)),
                    spacing,
                    range,
                    128,
                    order,
                    DirectVariant::Raycast,
                    ReflectionVariant::Convolution,
                ));
            }
        }
    }
    for direct in [
        DirectVariant::Raycast,
        DirectVariant::Volumetric05,
        DirectVariant::Volumetric10,
    ] {
        for reflection in [
            ReflectionVariant::Convolution,
            ReflectionVariant::Hybrid,
            ReflectionVariant::Parametric,
        ] {
            for block in [64, 128, 256, 512] {
                plan.push(runtime_spec(
                    format!(
                        "runtime-cross-d{}-x{}-b{block}",
                        direct_name(direct),
                        reflection_name(reflection)
                    ),
                    "local-s2-r100".into(),
                    2.0,
                    100.0,
                    block,
                    2,
                    direct,
                    reflection,
                ));
            }
        }
    }
    for (spacing, range, order, label) in
        [(0.5_f32, 250.0_f32, 3, "dense"), (4.0, 25.0, 0, "coarse")]
    {
        for block in [64, 512] {
            for reflection in [ReflectionVariant::Convolution, ReflectionVariant::Hybrid] {
                plan.push(runtime_spec(
                    format!(
                        "runtime-guard-{label}-b{block}-x{}",
                        reflection_name(reflection)
                    ),
                    format!("local-s{}-r{}", compact(spacing), compact(range)),
                    spacing,
                    range,
                    block,
                    order,
                    DirectVariant::Raycast,
                    reflection,
                ));
            }
        }
    }
    plan
}

fn bake_selected(mode: SweepMode, spec: &BakeSpec) -> bool {
    match mode {
        SweepMode::Sampled => {
            (spec.world == World::Local && spec.spacing_m == 2.0 && spec.path_range_m == 100.0)
                || (spec.world == World::Kilometer && spec.spacing_m == 100.0)
        }
        SweepMode::Full => {
            spec.world == World::Local
                || (spec.world == World::Kilometer
                    && (spec.spacing_m >= 50.0
                        || (spec.spacing_m == 25.0 && optional_25m_permitted(spec))))
        }
    }
}

fn optional_25m_permitted(spec: &BakeSpec) -> bool {
    spec.predicted_probes <= PROBE_CAP && spec.predicted_pairs <= PAIR_CAP
}

fn runtime_selected(mode: SweepMode, spec: &RuntimeSpec) -> bool {
    mode == SweepMode::Full
        || (spec.bake_id == "local-s2-r100"
            && ((spec.block_size == 128
                && spec.path_order == 2
                && spec.direct == DirectVariant::Raycast
                && spec.reflection == ReflectionVariant::Convolution)
                || (spec.block_size == 64
                    && spec.direct == DirectVariant::Volumetric05
                    && spec.reflection == ReflectionVariant::Hybrid)
                || (spec.block_size == 512
                    && spec.direct == DirectVariant::Volumetric10
                    && spec.reflection == ReflectionVariant::Parametric)))
}

fn canonical_plan_json(mode: SweepMode, protocol: &Protocol) -> serde_json::Value {
    let bakes: Vec<_> = bake_plan()
        .into_iter()
        .map(|spec| {
            serde_json::json!({
                "id": spec.id,
                "family": bake_family(&spec),
                "input": world_input(spec.world),
                "requested": bake_requested(&spec),
                "selected": bake_selected(mode, &spec),
            })
        })
        .collect();
    let runtimes: Vec<_> = runtime_plan()
        .into_iter()
        .map(|spec| {
            serde_json::json!({
                "id": spec.id,
                "family": "retained_runtime",
                "input": world_input(World::Local),
                "requested": runtime_requested(&spec, protocol),
                "selected": runtime_selected(mode, &spec),
            })
        })
        .collect();
    serde_json::json!({
        "mode": mode,
        "protocol": protocol,
        "bakes": bakes,
        "runtimes": runtimes,
    })
}

fn canonical_cases(mode: SweepMode, protocol: &Protocol) -> BTreeMap<String, ExpectedCase> {
    let mut expected = BTreeMap::new();
    for spec in bake_plan() {
        expected.insert(
            spec.id.clone(),
            ExpectedCase {
                family: bake_family(&spec),
                input: world_input(spec.world),
                requested: bake_requested(&spec),
                selected: bake_selected(mode, &spec),
            },
        );
    }
    for spec in runtime_plan() {
        expected.insert(
            spec.id.clone(),
            ExpectedCase {
                family: "retained_runtime",
                input: world_input(World::Local),
                requested: runtime_requested(&spec, protocol),
                selected: runtime_selected(mode, &spec),
            },
        );
    }
    expected
}

fn stage_authority_artifacts(
    root: &Path,
    mode: SweepMode,
    protocol: &Protocol,
    sdk: &SdkBinary,
) -> Result<AuthorityProvenance> {
    let sdk_path = sdk
        .dylib_path
        .as_ref()
        .ok_or_else(|| CliError::new("sweep SDK dylib path is not established"))?;
    let sdk_bytes = fs::read(sdk_path).map_err(io("read pinned SDK dylib"))?;
    let sdk_hash = sha256_hex(&sdk_bytes);
    if sdk_hash != PINNED_LIBPHONON_SHA256
        || sdk.dylib_checksum_sha256.as_deref() != Some(PINNED_LIBPHONON_SHA256)
    {
        return Err(CliError::new(
            "sweep SDK dylib does not match the exact pinned libphonon checksum",
        ));
    }
    let executable = std::env::current_exe().map_err(io("resolve sweep executable"))?;
    let executable_bytes = fs::read(&executable).map_err(io("read sweep executable"))?;
    fs::create_dir(root.join("authority")).map_err(io("create sweep authority directory"))?;
    write_bytes_atomic(&root.join(BUNDLED_LIBPHONON_PATH), &sdk_bytes)?;
    write_bytes_atomic(&root.join(BUNDLED_ENGINE_PATH), &executable_bytes)?;
    fs::set_permissions(
        root.join(BUNDLED_ENGINE_PATH),
        fs::metadata(&executable)
            .map_err(io("inspect sweep executable permissions"))?
            .permissions(),
    )
    .map_err(io("preserve bundled sweep executable permissions"))?;
    Ok(AuthorityProvenance {
        engine_identity: provenance::ENGINE_IDENTITY.into(),
        source_state: "unborn_main_uncommitted_source".into(),
        source_identity_sha256: source_identity_sha256()?,
        engine_executable_path: BUNDLED_ENGINE_PATH.into(),
        engine_executable_sha256: sha256_hex(&executable_bytes),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .into(),
        platform: provenance::platform().into(),
        cpu_class: provenance::cpu_class().into(),
        steam_audio_version: sdk.version.into(),
        steam_audio_upstream_commit: sdk.upstream_commit.into(),
        steam_audio_dylib_path: BUNDLED_LIBPHONON_PATH.into(),
        steam_audio_dylib_sha256: sdk_hash,
        sample_rate_hz: 48_000,
        canonical_plan_sha256: hash_json(&canonical_plan_json(mode, protocol))?,
    })
}

#[allow(clippy::too_many_arguments)]
fn runtime_spec(
    id: String,
    bake_id: String,
    spacing_m: f32,
    path_range_m: f32,
    block_size: i32,
    path_order: i32,
    direct: DirectVariant,
    reflection: ReflectionVariant,
) -> RuntimeSpec {
    RuntimeSpec {
        id,
        bake_id,
        spacing_m,
        path_range_m,
        block_size,
        path_order,
        direct,
        reflection,
    }
}

fn execute_bake(spec: &BakeSpec, dir: &Path) -> Result<CaseReport> {
    let requested = bake_requested(spec);
    let args = vec![
        "bake".into(),
        world_name(spec.world).into(),
        spec.spacing_m.to_string(),
        spec.path_range_m.to_string(),
        dir.display().to_string(),
    ];
    let run = run_isolated(&args, dir)?;
    let envelope = read_json::<ChildEnvelope>(&dir.join("child.json")).ok();
    if envelope.is_none() {
        discard_partial_bake_artifacts(dir)?;
    }
    let (mut status, mut reason) = child_status_and_reason(&run, envelope.as_ref(), "bake");
    let bake = envelope.and_then(|e| e.bake);
    if status == CaseStatus::Passed {
        if let Some(bake) = &bake {
            let within_budgets = run
                .resources
                .wall_time_ms
                .is_some_and(|v| v <= POSITIVE_BAKE_LIMIT_MS)
                && peak_rss(&run.resources).is_some_and(|v| v <= MAX_RSS_BUDGET)
                && bake.metadata.path_data_size_bytes <= PATH_DATA_BUDGET
                && bake.metadata.serialized_size_bytes <= PACKAGE_BUDGET;
            if spec.positive && !within_budgets {
                status = CaseStatus::QualityFailed;
                reason = Some("positive bake exceeded one or more viability budgets".into());
            }
        } else {
            status = CaseStatus::Error;
            reason = Some("passed child omitted bake evidence".into());
        }
    }
    let bake_delivered = bake.as_ref().map(|b| {
        serde_json::json!({
            "settings": requested,
            "metadata": b.metadata,
        })
    });
    Ok(CaseReport {
        id: spec.id.clone(),
        family: bake_family(spec).into(),
        input_hash: hash_json(&world_input(spec.world))?,
        configuration_hash: hash_json(&requested)?,
        status,
        reason,
        requested,
        bake_delivered,
        runtime_delivered: None,
        resources: run.resources,
        validation_status: None,
        validation_reason: None,
        validation_resources: None,
        probe_count: bake.as_ref().map(|b| b.metadata.probe_count),
        path_data_bytes: bake.as_ref().map(|b| b.metadata.path_data_size_bytes),
        serialized_package_bytes: bake.as_ref().map(|b| b.metadata.serialized_size_bytes),
        package_sha256: bake.as_ref().map(|b| b.metadata.content_sha256.clone()),
        benchmark: None,
        validation: None,
    })
}

fn discard_partial_bake_artifacts(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)
        .map_err(io("read failed bake directory for partial cleanup"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(io("read failed bake artifact entry"))?
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let final_partial = matches!(
            name.as_ref(),
            "probe-batch.bin" | "bake.json" | "child.json"
        );
        let interrupted_atomic = name.starts_with(".probe-batch.bin.tmp.")
            || name.starts_with(".bake.json.tmp.")
            || name.starts_with(".child.json.tmp.");
        if final_partial || interrupted_atomic {
            let path = entry.path();
            if entry
                .file_type()
                .map_err(io("inspect failed bake partial artifact"))?
                .is_dir()
            {
                fs::remove_dir_all(path).map_err(io("remove failed bake partial directory"))?;
            } else {
                fs::remove_file(path).map_err(io("remove failed bake partial artifact"))?;
            }
        }
    }
    Ok(())
}

fn execute_validation(spec: &BakeSpec, dir: &Path) -> Result<(RunOutcome, Option<ChildEnvelope>)> {
    let args = vec![
        "validate".into(),
        world_name(spec.world).into(),
        spec.spacing_m.to_string(),
        spec.path_range_m.to_string(),
        dir.display().to_string(),
    ];
    let run = run_isolated(&args, dir)?;
    let envelope = read_json(&dir.join("validation-child.json")).ok();
    Ok((run, envelope))
}

fn execute_runtime(
    spec: &RuntimeSpec,
    bake_dir: &Path,
    dir: &Path,
    protocol: &Protocol,
) -> Result<CaseReport> {
    let requested = runtime_requested(spec, protocol);
    let iterations = protocol.iterations();
    let args = vec![
        "benchmark".into(),
        spec.spacing_m.to_string(),
        spec.path_range_m.to_string(),
        spec.block_size.to_string(),
        spec.path_order.to_string(),
        direct_name(spec.direct).into(),
        reflection_name(spec.reflection).into(),
        iterations.simulation_warmup.to_string(),
        iterations.simulation_measured.to_string(),
        iterations.reflection_warmup.to_string(),
        iterations.reflection_measured.to_string(),
        iterations.effect_warmup.to_string(),
        iterations.effect_measured.to_string(),
        bake_dir.display().to_string(),
        dir.display().to_string(),
    ];
    let run = run_isolated(&args, dir)?;
    let envelope = read_json::<ChildEnvelope>(&dir.join("child.json")).ok();
    let (status, reason) = child_status_and_reason(&run, envelope.as_ref(), "runtime");
    let bake = read_json::<BakeWire>(&bake_dir.join("bake.json")).ok();
    Ok(CaseReport {
        id: spec.id.clone(),
        family: "retained_runtime".into(),
        input_hash: hash_json(&world_input(World::Local))?,
        configuration_hash: hash_json(&requested)?,
        status,
        reason,
        requested,
        bake_delivered: None,
        runtime_delivered: envelope.as_ref().and_then(|e| e.delivered.clone()),
        resources: run.resources,
        validation_status: None,
        validation_reason: None,
        validation_resources: None,
        probe_count: bake.as_ref().map(|b| b.metadata.probe_count),
        path_data_bytes: bake.as_ref().map(|b| b.metadata.path_data_size_bytes),
        serialized_package_bytes: bake.as_ref().map(|b| b.metadata.serialized_size_bytes),
        package_sha256: bake.as_ref().map(|b| b.metadata.content_sha256.clone()),
        benchmark: envelope.and_then(|e| e.benchmark),
        validation: None,
    })
}

struct RunOutcome {
    resources: Resources,
    reason: Option<String>,
}

fn run_isolated(args: &[String], dir: &Path) -> Result<RunOutcome> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().map_err(io("resolve current executable"))?;
    let kind = args.first().map(String::as_str).unwrap_or("child");
    let log = dir.join(format!("time-{kind}.log"));
    let stderr = fs::File::create(&log).map_err(io("create child time log"))?;
    let mut command = Command::new("/usr/bin/time");
    command
        .arg("-l")
        .arg(exe)
        .arg("phase-a")
        .arg("__sweep-child")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(io("spawn isolated sweep process group"))?;
    supervise(&mut child, dir, &log)
}

fn supervise(child: &mut Child, dir: &Path, log: &Path) -> Result<RunOutcome> {
    let started = Instant::now();
    let process_group = child.id();
    let mut rss_samples = Vec::new();
    let mut high_rss_samples = 0_u8;
    let mut peak_temp = 0_u64;
    let mut forced = None;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(io("poll sweep child"))? {
            break status;
        }
        if let Some(rss) = process_tree_rss(child.id()) {
            rss_samples.push(rss);
            high_rss_samples = if rss >= LIVE_RSS_KILL {
                high_rss_samples.saturating_add(1)
            } else {
                0
            };
        }
        let temp_size = directory_size(dir).unwrap_or(u64::MAX);
        peak_temp = peak_temp.max(temp_size);
        if high_rss_samples >= 3 {
            forced = Some((
                "resource_killed",
                "three consecutive 100ms RSS samples were at least 8 GiB",
            ));
        } else if temp_size > TEMP_CAP {
            forced = Some(("resource_killed", "case temporary directory exceeded 1 GiB"));
        } else if started.elapsed() >= Duration::from_millis(CHILD_TIMEOUT_MS) {
            forced = Some(("timeout", "isolated child exceeded 15 minute timeout"));
        }
        if forced.is_some() {
            terminate_group(process_group);
            break child
                .wait()
                .map_err(io("wait for terminated process group"))?;
        }
        thread::sleep(Duration::from_millis(100));
    };
    if !group_members(process_group).is_empty() {
        terminate_group(process_group);
        if !group_members(process_group).is_empty() {
            forced = Some((
                "error",
                "process group still contained live descendants after TERM/KILL cleanup",
            ));
        } else if forced.is_none() {
            forced = Some((
                "error",
                "child leader exited while descendants survived; descendants were killed",
            ));
        }
    }
    // A fast child can write its largest file between 100 ms samples. Observe
    // disk only after the leader and all descendants have exited, and include
    // the completed `/usr/bin/time` log in the final peak.
    let final_temp_size = directory_size(dir).unwrap_or(u64::MAX);
    peak_temp = peak_temp.max(final_temp_size);
    apply_final_disk_cap(&mut forced, final_temp_size);
    use std::os::unix::process::ExitStatusExt;
    let time_text = fs::read_to_string(log).unwrap_or_default();
    Ok(RunOutcome {
        resources: Resources {
            wall_time_ms: Some(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
            child_exit_code: status.code(),
            child_signal: status.signal(),
            time_l_max_rss_bytes: parse_time_l_max_rss(&time_text),
            peak_sampled_rss_bytes: rss_samples.iter().copied().max(),
            sampled_rss_bytes: rss_samples,
            temp_directory_peak_bytes: Some(peak_temp),
            termination: forced.map(|v| v.0.into()),
        },
        reason: forced.map(|v| v.1.into()),
    })
}

fn apply_final_disk_cap(forced: &mut Option<(&'static str, &'static str)>, final_temp_size: u64) {
    if final_temp_size > TEMP_CAP {
        *forced = Some((
            "resource_killed",
            "completed case directory exceeded the 1 GiB cap",
        ));
    }
}

fn terminate_group(process_group: u32) {
    terminate_group_with_grace(process_group, Duration::from_secs(2));
}

fn terminate_group_with_grace(process_group: u32, term_grace: Duration) {
    let group = format!("-{process_group}");
    let _ = Command::new("/bin/kill")
        .args(["-TERM", group.as_str()])
        .status();
    signal_group_members("-TERM", process_group);
    let deadline = Instant::now() + term_grace;
    while Instant::now() < deadline {
        if group_members(process_group).is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = Command::new("/bin/kill")
        .args(["-KILL", group.as_str()])
        .status();
    let kill_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < kill_deadline {
        if group_members(process_group).is_empty() {
            return;
        }
        signal_group_members("-KILL", process_group);
        thread::sleep(Duration::from_millis(25));
    }
}

fn signal_group_members(signal: &str, process_group: u32) {
    let Some(members) = try_group_members(process_group) else {
        return;
    };
    if members.is_empty() {
        return;
    }
    let mut command = Command::new("/bin/kill");
    command.arg(signal);
    for member in members {
        command.arg(member.to_string());
    }
    let _ = command.status();
}

fn group_members(process_group: u32) -> Vec<u32> {
    try_group_members(process_group).unwrap_or_else(|| {
        process_group_exists(process_group)
            .then_some(process_group)
            .into_iter()
            .collect()
    })
}

fn process_group_exists(process_group: u32) -> bool {
    let group = format!("-{process_group}");
    Command::new("/bin/kill")
        .args(["-0", group.as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(true)
}

fn try_group_members(process_group: u32) -> Option<Vec<u32>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,state="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse::<u32>().ok()?;
                let pgid = fields.next()?.parse::<u32>().ok()?;
                let state = fields.next()?;
                (pgid == process_group && !state.starts_with('Z')).then_some(pid)
            })
            .collect(),
    )
}

fn process_tree_rss(root: u32) -> Option<u64> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pgid=,rss=,state="])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pgid = fields.next()?.parse::<u32>().ok()?;
            let rss = fields.next()?.parse::<u64>().ok()?;
            let state = fields.next()?;
            (pgid == root && !state.starts_with('Z')).then_some(rss)
        })
        .try_fold(0_u64, |sum, kib| sum.checked_add(kib.checked_mul(1024)?))
}

fn directory_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        total = total.saturating_add(if metadata.is_dir() {
            directory_size(&entry.path())?
        } else {
            metadata.len()
        });
    }
    Ok(total)
}

fn parse_time_l_max_rss(text: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.contains("maximum resident set size"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

fn classify(run: &RunOutcome, envelope: Option<&ChildEnvelope>) -> CaseStatus {
    match run.resources.termination.as_deref() {
        Some("timeout") => CaseStatus::Timeout,
        Some("resource_killed") => CaseStatus::ResourceKilled,
        Some("error") => CaseStatus::Error,
        _ => envelope.map(|e| e.status).unwrap_or(CaseStatus::Error),
    }
}

fn child_status_and_reason(
    run: &RunOutcome,
    envelope: Option<&ChildEnvelope>,
    kind: &str,
) -> (CaseStatus, Option<String>) {
    let status = classify(run, envelope);
    let reason = run
        .reason
        .clone()
        .or_else(|| envelope.and_then(|value| value.reason.clone()))
        .or_else(|| {
            envelope
                .is_none()
                .then_some(format!("{kind} child produced no readable result envelope"))
        });
    (status, reason)
}

pub fn run_child(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("bake") => child_bake(args),
        Some("validate") => child_validate(args),
        Some("benchmark") => child_benchmark(args),
        _ => Err(CliError::new("invalid internal sweep child invocation")),
    }
}

fn child_bake(args: &[String]) -> Result<()> {
    if args.len() != 5 {
        return Err(CliError::new("internal bake child argument count mismatch"));
    }
    let world = parse_world(&args[1])?;
    let spacing = number::<f32>(&args[2], "spacing")?;
    let range = number::<f32>(&args[3], "path range")?;
    let out = Path::new(&args[4]);
    let envelope = match bake_s3(&bake_request(world, spacing, range)) {
        Ok(baked) => {
            write_bytes_atomic(&out.join("probe-batch.bin"), &baked.bytes)?;
            let wire = BakeWire {
                metadata: MetadataWire {
                    probe_count: baked.metadata.probe_count,
                    path_data_size_bytes: baked.metadata.path_data_size_bytes,
                    serialized_size_bytes: baked.metadata.serialized_size_bytes,
                    content_sha256: baked.metadata.content_sha256,
                    bake_progress_callback_count: baked.metadata.bake_progress_callback_count,
                    final_bake_progress_millionths: baked.metadata.final_bake_progress_millionths,
                },
            };
            write_json_atomic(&out.join("bake.json"), &wire)?;
            ChildEnvelope {
                status: CaseStatus::Passed,
                reason: None,
                bake: Some(wire),
                benchmark: None,
                validation: None,
                delivered: None,
            }
        }
        Err(error) => ChildEnvelope {
            status: if error.to_string().contains("no path")
                || error.to_string().contains("no PATHING")
            {
                CaseStatus::NoPath
            } else {
                CaseStatus::Error
            },
            reason: Some(error.to_string()),
            bake: None,
            benchmark: None,
            validation: None,
            delivered: None,
        },
    };
    write_json_atomic(&out.join("child.json"), &envelope)
}

fn child_validate(args: &[String]) -> Result<()> {
    if args.len() != 5 {
        return Err(CliError::new(
            "internal validation child argument count mismatch",
        ));
    }
    let world = parse_world(&args[1])?;
    let spacing = number::<f32>(&args[2], "spacing")?;
    let range = number::<f32>(&args[3], "path range")?;
    let dir = Path::new(&args[4]);
    let baked = load_baked(dir)?;
    let request = render_request(
        world,
        spacing,
        range,
        128,
        1,
        DirectVariant::Raycast,
        ReflectionVariant::Convolution,
    );
    let envelope = match render_s3(&request, &baked) {
        Ok(output) => {
            let path = &output.snapshot.path;
            let finite = path.sh_coeffs.iter().all(|v| v.is_finite())
                && path.eq_coeffs.iter().all(|v| v.is_finite());
            let nonzero = path
                .sh_coeffs
                .iter()
                .chain(path.eq_coeffs.iter())
                .any(|v| v.abs() > f32::EPSILON);
            let delivered = path
                .direction
                .map(|value| f64::from(value.azimuth_degrees_clockwise_from_north));
            let delta = delivered.map(|value| angular_delta(value, ANALYTIC_AZIMUTH_DEGREES));
            let analytic_passed = delta.is_some_and(|value| value <= 5.0);
            let occluded = path
                .validation_segments
                .iter()
                .filter(|segment| segment.occluded)
                .count();
            let validation = ValidationEvidence {
                fresh_process: true,
                path_output_nonzero: nonzero,
                path_output_finite: finite,
                analytic_arrival_degrees: ANALYTIC_AZIMUTH_DEGREES,
                delivered_arrival_degrees: delivered,
                angular_error_degrees: delta,
                analytic_arrival_passed: analytic_passed,
                validation_segments: path.validation_segments.len(),
                occluded_validation_segments: occluded,
                requested_settings: settings_json(&request.simulation, 128),
                delivered_settings: {
                    let mut delivered = request.simulation;
                    delivered.direct_occlusion = output.snapshot.direct.delivered_occlusion_mode;
                    delivered.reflection_effect.effect_type =
                        output.snapshot.reflections.delivered_effect_type;
                    delivered.pathing_order = output.snapshot.path.configured_order;
                    settings_json(&delivered, 128)
                },
            };
            let passed = nonzero && finite && analytic_passed && occluded == 0;
            let deliberate_negative = world == World::Kilometer && range <= 1_000.0;
            ChildEnvelope {
                status: if passed {
                    CaseStatus::Passed
                } else if deliberate_negative || !nonzero {
                    CaseStatus::NoPath
                } else {
                    CaseStatus::QualityFailed
                },
                reason: (!passed).then_some(
                    "fresh-process validation failed nonzero/finite/analytic evidence".into(),
                ),
                bake: None,
                benchmark: None,
                validation: Some(validation),
                delivered: Some(settings_json(&request.simulation, 128)),
            }
        }
        Err(error) => ChildEnvelope {
            status: CaseStatus::Error,
            reason: Some(error.to_string()),
            bake: None,
            benchmark: None,
            validation: None,
            delivered: None,
        },
    };
    write_json_atomic(&dir.join("validation-child.json"), &envelope)
}

fn child_benchmark(args: &[String]) -> Result<()> {
    if args.len() != 15 {
        return Err(CliError::new(format!(
            "internal benchmark child argument count mismatch: {}",
            args.len()
        )));
    }
    let spacing = number::<f32>(&args[1], "spacing")?;
    let range = number::<f32>(&args[2], "range")?;
    let block = number::<i32>(&args[3], "block size")?;
    let order = number::<i32>(&args[4], "path order")?;
    let direct = parse_direct(&args[5])?;
    let reflection = parse_reflection(&args[6])?;
    let iterations = S3BenchmarkIterations {
        simulation_warmup: number(&args[7], "simulation warmup")?,
        simulation_measured: number(&args[8], "simulation measured")?,
        reflection_warmup: number(&args[9], "reflection warmup")?,
        reflection_measured: number(&args[10], "reflection measured")?,
        effect_warmup: number(&args[11], "effect warmup")?,
        effect_measured: number(&args[12], "effect measured")?,
    };
    let baked = load_baked(Path::new(&args[13]))?;
    let out = Path::new(&args[14]);
    let render = render_request(
        World::Local,
        spacing,
        range,
        block,
        order,
        direct,
        reflection,
    );
    let envelope = match benchmark_s3_stages(&S3BenchmarkRequest { render, iterations }, &baked) {
        Ok(output) => {
            let evidence = benchmark_evidence(&output, block)?;
            ChildEnvelope {
                status: if evidence.budget_result == "hard_limit_exceeded" {
                    CaseStatus::QualityFailed
                } else {
                    CaseStatus::Passed
                },
                reason: (evidence.budget_result == "hard_limit_exceeded")
                    .then_some("one or more retained offline stage hard limits exceeded".into()),
                delivered: Some(evidence.delivered_settings.clone()),
                benchmark: Some(evidence),
                validation: None,
                bake: None,
            }
        }
        Err(error) => ChildEnvelope {
            status: CaseStatus::Error,
            reason: Some(error.to_string()),
            bake: None,
            benchmark: None,
            validation: None,
            delivered: None,
        },
    };
    write_json_atomic(&out.join("child.json"), &envelope)
}

fn benchmark_evidence(output: &S3BenchmarkOutput, block: i32) -> Result<BenchmarkEvidence> {
    let f = output.finite;
    if !(f.direct_simulation
        && f.path_simulation
        && f.reflection_simulation
        && f.direct_effect_binaural_apply
        && f.path_effect_apply
        && f.reflection_effect_decode_apply)
        || f.direct_simulation_samples_checked != output.iterations.simulation_measured
        || f.path_simulation_samples_checked != output.iterations.simulation_measured
        || f.reflection_simulation_samples_checked != output.iterations.reflection_measured
        || f.direct_effect_samples_checked != output.iterations.effect_measured
        || f.path_effect_samples_checked != output.iterations.effect_measured
        || f.reflection_effect_samples_checked != output.iterations.effect_measured
    {
        return Err(CliError::new(
            "backend finite-check counters do not equal measured iteration counts",
        ));
    }
    if output.retained.rendered_blocks
        != output.iterations.effect_warmup + output.iterations.effect_measured
    {
        return Err(CliError::new(
            "retained rendered_blocks does not equal effect warmup + measured",
        ));
    }
    let s = &output.samples;
    let direct = stage(&s.direct_simulation_ns)?;
    let path = stage(&s.path_simulation_ns)?;
    let reflection = stage(&s.reflection_simulation_ns)?;
    let hard = direct.derived.p99_ns > 16_700_000
        || path.derived.p99_ns > 66_700_000
        || reflection.derived.p99_ns > 200_000_000;
    let target = direct.derived.p99_ns > 8_000_000
        || path.derived.p99_ns > 20_000_000
        || reflection.derived.p99_ns > 100_000_000;
    Ok(BenchmarkEvidence {
        loaded_probe_count: output.loaded_probe_count,
        loaded_path_data_size_bytes: output.loaded_path_data_size_bytes,
        retained_rendered_blocks: output.retained.rendered_blocks,
        requested_settings: settings_json(&output.requested_simulation, block),
        delivered_settings: settings_json(&output.delivered_simulation, block),
        direct_simulation: direct,
        path_simulation: path,
        reflection_simulation: reflection,
        direct_effect_binaural_apply: stage(&s.direct_effect_binaural_apply_ns)?,
        path_effect_apply: stage(&s.path_effect_apply_ns)?,
        reflection_effect_decode_apply: stage(&s.reflection_effect_decode_apply_ns)?,
        finite: FiniteEvidence {
            direct_simulation: f.direct_simulation,
            path_simulation: f.path_simulation,
            reflection_simulation: f.reflection_simulation,
            direct_effect_binaural_apply: f.direct_effect_binaural_apply,
            path_effect_apply: f.path_effect_apply,
            reflection_effect_decode_apply: f.reflection_effect_decode_apply,
            direct_simulation_samples_checked: f.direct_simulation_samples_checked,
            path_simulation_samples_checked: f.path_simulation_samples_checked,
            reflection_simulation_samples_checked: f.reflection_simulation_samples_checked,
            direct_effect_samples_checked: f.direct_effect_samples_checked,
            path_effect_samples_checked: f.path_effect_samples_checked,
            reflection_effect_samples_checked: f.reflection_effect_samples_checked,
        },
        budget_result: if hard {
            "hard_limit_exceeded"
        } else if target {
            "target_exceeded"
        } else {
            "within_targets"
        }
        .into(),
    })
}

fn stage(raw: &[u64]) -> Result<Stage> {
    Ok(Stage {
        raw_ns: raw.to_vec(),
        derived: percentiles(raw)?,
    })
}

fn percentiles(raw: &[u64]) -> Result<Percentiles> {
    if raw.is_empty() {
        return Err(CliError::new("cannot derive percentiles from zero samples"));
    }
    let mut sorted = raw.to_vec();
    sorted.sort_unstable();
    let pick = |percent: usize| {
        let rank = (percent * sorted.len()).div_ceil(100).max(1);
        sorted[rank - 1]
    };
    Ok(Percentiles {
        n: sorted.len(),
        p50_ns: pick(50),
        p95_ns: pick(95),
        p99_ns: pick(99),
        max_ns: *sorted.last().unwrap(),
    })
}

fn bake_request(world: World, spacing: f32, range: f32) -> S3BakeRequest {
    let (mesh, probes) = match world {
        World::Local => (
            local_mesh(),
            ProbeVolume {
                min_enu_m: EnuVector3::new(-8.75, -8.75, 0.5),
                max_enu_m: EnuVector3::new(8.25, 8.25, 2.5),
                spacing_m: spacing,
                height_above_floor_m: 1.5,
            },
        ),
        World::Kilometer => (
            kilometer_mesh(),
            ProbeVolume {
                min_enu_m: EnuVector3::new(-450.0 + spacing / 4.0, -450.0 + spacing / 4.0, 0.5),
                max_enu_m: EnuVector3::new(650.0 + spacing / 4.0, 650.0 + spacing / 4.0, 2.5),
                spacing_m: spacing,
                height_above_floor_m: 1.5,
            },
        ),
    };
    S3BakeRequest {
        mesh,
        probes,
        elevated_probe_layers: Vec::new(),
        pathing: PathBakeConfig {
            num_visibility_samples: 1,
            probe_visibility_radius_m: 0.0,
            visibility_threshold: 0.5,
            visibility_range_m: if world == World::Kilometer {
                2.5 * spacing
            } else {
                6.0
            },
            path_range_m: range,
            num_threads: 4,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn render_request(
    world: World,
    spacing: f32,
    range: f32,
    block: i32,
    order: i32,
    direct: DirectVariant,
    reflection: ReflectionVariant,
) -> S3RenderRequest {
    let bake = bake_request(world, spacing, range);
    let direct_occlusion = match direct {
        DirectVariant::Raycast => DirectOcclusionMode::Raycast,
        DirectVariant::Volumetric05 => DirectOcclusionMode::Volumetric {
            radius_m: 0.5,
            sample_count: 16,
        },
        DirectVariant::Volumetric10 => DirectOcclusionMode::Volumetric {
            radius_m: 1.0,
            sample_count: 64,
        },
    };
    let reflection_effect = match reflection {
        ReflectionVariant::Convolution => ReflectionEffectConfig::CONVOLUTION,
        ReflectionVariant::Hybrid => ReflectionEffectConfig::hybrid(0.25, 0.25),
        ReflectionVariant::Parametric => ReflectionEffectConfig::PARAMETRIC,
    };
    let simulation = S3SimulationConfig {
        max_occlusion_samples: 64,
        direct_occlusion,
        reflection_duration_s: 1.0,
        reflection_order: 1,
        reflection_effect,
        pathing_order: order,
        pathing_visibility_range_m: if world == World::Kilometer {
            2.5 * spacing
        } else {
            6.0
        },
        trace_path_validation: world == World::Kilometer,
        ..S3SimulationConfig::default()
    };
    let (source, listener) = match world {
        World::Local => (
            EnuVector3::new(-4.0, 6.0, 1.5),
            ListenerPose::at(EnuVector3::new(6.0, -4.0, 1.5)),
        ),
        World::Kilometer => (
            EnuVector3::new(-400.0, 600.0, 1.5),
            ListenerPose::at(EnuVector3::new(600.0, -400.0, 1.5)),
        ),
    };
    S3RenderRequest {
        mesh: bake.mesh,
        audio: AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: block,
        },
        simulation,
        source_position_enu: source,
        listener,
        input_mono: vec![0.01; block as usize],
        calibration_gain: 1.0,
    }
}

fn local_mesh() -> SceneMesh {
    SceneMesh {
        vertices_enu_m: vec![
            EnuVector3::new(0.0, 0.0, 0.0),
            EnuVector3::new(10.0, 0.0, 0.0),
            EnuVector3::new(10.0, 0.0, 6.0),
            EnuVector3::new(0.0, 0.0, 6.0),
            EnuVector3::new(0.0, 10.0, 0.0),
            EnuVector3::new(0.0, 10.0, 6.0),
            EnuVector3::new(-9.0, -9.0, 0.0),
            EnuVector3::new(9.0, -9.0, 0.0),
            EnuVector3::new(9.0, 9.0, 0.0),
            EnuVector3::new(-9.0, 9.0, 0.0),
        ],
        triangles: vec![
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
        ],
        material_indices: vec![0; 10],
        materials: vec![AcousticMaterial::MASONRY],
    }
}

fn kilometer_mesh() -> SceneMesh {
    SceneMesh {
        vertices_enu_m: vec![
            EnuVector3::new(0.0, 0.0, 0.0),
            EnuVector3::new(700.0, 0.0, 0.0),
            EnuVector3::new(700.0, 0.0, 100.0),
            EnuVector3::new(0.0, 0.0, 100.0),
            EnuVector3::new(0.0, 700.0, 0.0),
            EnuVector3::new(0.0, 700.0, 100.0),
            EnuVector3::new(-470.0, -470.0, 0.0),
            EnuVector3::new(680.0, -470.0, 0.0),
            EnuVector3::new(680.0, 680.0, 0.0),
            EnuVector3::new(-470.0, 680.0, 0.0),
        ],
        triangles: vec![
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
            [8, 7, 6],
            [9, 8, 6],
        ],
        material_indices: vec![0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1],
        materials: vec![AcousticMaterial::MASONRY, AcousticMaterial::GROUND],
    }
}

fn load_baked(dir: &Path) -> Result<BakedProbeBatch> {
    let wire: BakeWire = read_json(&dir.join("bake.json"))?;
    let bytes = fs::read(dir.join("probe-batch.bin")).map_err(io("read serialized probe batch"))?;
    let m = wire.metadata;
    let baked = BakedProbeBatch {
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: m.probe_count,
            path_data_size_bytes: m.path_data_size_bytes,
            serialized_size_bytes: m.serialized_size_bytes,
            content_sha256: m.content_sha256,
            bake_progress_callback_count: m.bake_progress_callback_count,
            final_bake_progress_millionths: m.final_bake_progress_millionths,
        },
        bytes,
    };
    baked
        .validate()
        .map_err(|error| CliError::new(format!("invalid child bake evidence: {error}")))?;
    Ok(baked)
}

fn build_manifest(root: &Path) -> Result<Manifest> {
    let mut artifacts = Vec::new();
    collect_artifacts(root, root, &mut artifacts)?;
    artifacts.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(Manifest {
        schema_version: ARTIFACT_SCHEMA.into(),
        artifacts,
    })
}

fn collect_artifacts(root: &Path, current: &Path, out: &mut Vec<Artifact>) -> Result<()> {
    for entry in fs::read_dir(current).map_err(io("scan sweep artifacts"))? {
        let entry = entry.map_err(io("read sweep artifact entry"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(io("inspect sweep artifact"))?;
        if metadata.file_type().is_symlink() {
            return Err(CliError::new(format!(
                "sweep bundles may not contain symlinks: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_artifacts(root, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if relative == "artifacts.json" {
            continue;
        }
        let bytes = fs::read(&path).map_err(io("hash sweep artifact"))?;
        out.push(Artifact {
            relative_path: relative,
            size_bytes: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
        });
    }
    Ok(())
}

fn verify_report(root: &Path) -> Result<()> {
    if fs::symlink_metadata(root)
        .map_err(io("inspect sweep report directory"))?
        .file_type()
        .is_symlink()
    {
        return Err(CliError::new(
            "sweep report directory itself may not be a symlink",
        ));
    }
    let root = root
        .canonicalize()
        .map_err(io("canonicalize sweep report directory"))?;
    verify_staged(&root)
}

fn verify_staged(root: &Path) -> Result<()> {
    let report: Report = read_json(&root.join("report.json"))?;
    let manifest: Manifest = read_json(&root.join("artifacts.json"))?;
    if report.schema_version != SCHEMA || manifest.schema_version != ARTIFACT_SCHEMA {
        return Err(CliError::new("unsupported sweep report schema"));
    }
    if manifest != build_manifest(root)? {
        return Err(CliError::new(
            "sweep artifact manifest does not match self-contained files",
        ));
    }
    if report.protocol != Protocol::for_mode(report.mode) {
        return Err(CliError::new(
            "sweep report protocol does not exactly match its mode",
        ));
    }
    verify_authority(root, &report.authority, report.mode, &report.protocol)?;
    verify_canonical_cases(&report)?;
    verify_artifact_layout(root, &report, &manifest)?;
    for case in &report.cases {
        verify_case_artifacts(root, case)?;
    }
    verify_decision(&report)
}

fn verify_canonical_cases(report: &Report) -> Result<()> {
    let expected = canonical_cases(report.mode, &report.protocol);
    if expected.len() != 116 || report.cases.len() != expected.len() {
        return Err(CliError::new(
            "sweep report does not contain the exact 116-case canonical plan",
        ));
    }
    let mut actual = BTreeMap::new();
    for case in &report.cases {
        if actual.insert(case.id.clone(), case).is_some() {
            return Err(CliError::new(format!(
                "duplicate sweep case id {:?}",
                case.id
            )));
        }
    }
    if actual.keys().ne(expected.keys()) {
        return Err(CliError::new(
            "sweep report has missing, extra, or relabelled canonical case ids",
        ));
    }
    for (id, canonical) in &expected {
        let case = actual[id];
        if case.family != canonical.family
            || case.requested != canonical.requested
            || case.input_hash != hash_json(&canonical.input)?
            || case.configuration_hash != hash_json(&canonical.requested)?
        {
            return Err(CliError::new(format!(
                "case {id} does not match immutable canonical membership/settings"
            )));
        }
        if !canonical.selected && case.status != CaseStatus::ProjectedSkip {
            return Err(CliError::new(format!(
                "canonical unselected case {id} must be projected_skip"
            )));
        }
        if canonical.selected && case.status == CaseStatus::ProjectedSkip {
            let dependency_failed = case.family == "retained_runtime"
                && case.reason.as_deref() == Some("required bake did not pass")
                && case.requested["bake_id"]
                    .as_str()
                    .and_then(|bake_id| actual.get(bake_id))
                    .is_some_and(|bake| bake.status != CaseStatus::Passed);
            if !dependency_failed {
                return Err(CliError::new(format!(
                    "canonically selected case {id} was projected without a failed bake dependency"
                )));
            }
        }
        verify_case(case, report.mode, &report.protocol)?;
    }
    Ok(())
}

fn verify_decision(report: &Report) -> Result<()> {
    if report.decision != formal_decision(report.mode, &report.cases, DECISION_REQUIRED_BY) {
        return Err(CliError::new(
            "formal kilometer decision does not follow case evidence",
        ));
    }
    Ok(())
}

fn verify_artifact_layout(root: &Path, report: &Report, manifest: &Manifest) -> Result<()> {
    use std::collections::BTreeSet;

    let mut allowed = BTreeSet::from([
        "report.json".to_string(),
        report.authority.steam_audio_dylib_path.clone(),
        report.authority.engine_executable_path.clone(),
    ]);
    for case in &report.cases {
        let case_dir = root.join("cases").join(&case.id);
        if case.status == CaseStatus::ProjectedSkip {
            if case_dir.exists() {
                return Err(CliError::new(format!(
                    "projected case {} must not have an artifact directory",
                    case.id
                )));
            }
            continue;
        }
        if !case_dir.is_dir() {
            return Err(CliError::new(format!(
                "attempted case {} lacks its artifact directory",
                case.id
            )));
        }
        let prefix = format!("cases/{}/", case.id);
        let kind = if case.family == "retained_runtime" {
            "benchmark"
        } else {
            "bake"
        };
        allowed.insert(format!("{prefix}time-{kind}.log"));
        allowed.insert(format!("{prefix}child.json"));
        if case.family != "retained_runtime" && case.package_sha256.is_some() {
            allowed.insert(format!("{prefix}bake.json"));
            allowed.insert(format!("{prefix}probe-batch.bin"));
        }
        if case.validation_status.is_some() {
            allowed.insert(format!("{prefix}time-validate.log"));
            allowed.insert(format!("{prefix}validation-child.json"));
        }
    }
    for artifact in &manifest.artifacts {
        if !allowed.contains(&artifact.relative_path) {
            return Err(CliError::new(format!(
                "unexpected sweep artifact {}",
                artifact.relative_path
            )));
        }
    }
    for required in allowed {
        if required.ends_with("child.json") || required.ends_with("validation-child.json") {
            continue;
        }
        if !manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.relative_path == required)
        {
            return Err(CliError::new(format!(
                "missing required sweep artifact {required}"
            )));
        }
    }
    Ok(())
}

fn verify_case(case: &CaseReport, mode: SweepMode, protocol: &Protocol) -> Result<()> {
    verify_resources(&case.id, &case.resources)?;
    if let Some(resources) = &case.validation_resources {
        verify_resources(&format!("{} validation", case.id), resources)?;
    }
    if case.status == CaseStatus::ProjectedSkip {
        if case.reason.is_none()
            || case.bake_delivered.is_some()
            || case.runtime_delivered.is_some()
            || case.benchmark.is_some()
            || case.validation.is_some()
            || case.validation_status.is_some()
            || case.validation_reason.is_some()
            || case.validation_resources.is_some()
            || case.probe_count.is_some()
            || case.path_data_bytes.is_some()
            || case.serialized_package_bytes.is_some()
            || case.package_sha256.is_some()
            || case.resources != Resources::unrun()
        {
            return Err(CliError::new(format!(
                "projected/unrun case {} contains attempted evidence",
                case.id
            )));
        }
        return Ok(());
    }
    if case.resources.wall_time_ms.is_none() {
        return Err(CliError::new(format!(
            "attempted case {} lacks child resource evidence",
            case.id
        )));
    }
    verify_exit_status(
        case.status,
        &case.resources,
        case.reason.as_deref(),
        &case.id,
    )?;
    let package_fields = [
        case.probe_count.is_some(),
        case.path_data_bytes.is_some(),
        case.serialized_package_bytes.is_some(),
        case.package_sha256.is_some(),
    ];
    if package_fields.iter().any(|value| *value) && !package_fields.iter().all(|value| *value) {
        return Err(CliError::new(format!(
            "case {} has a partial package evidence tuple",
            case.id
        )));
    }
    if case.family == "retained_runtime" {
        if case.bake_delivered.is_some()
            || case.validation.is_some()
            || case.validation_status.is_some()
            || case.validation_reason.is_some()
            || case.validation_resources.is_some()
            || case.status == CaseStatus::NoPath
            || (case.benchmark.is_none() && case.runtime_delivered.is_some())
        {
            return Err(CliError::new(format!(
                "runtime case {} has evidence/status from another family",
                case.id
            )));
        }
    } else if case.runtime_delivered.is_some() || case.benchmark.is_some() {
        return Err(CliError::new(format!(
            "bake case {} contains runtime benchmark evidence",
            case.id
        )));
    }
    if case.family != "kilometer_bake"
        && (case.validation.is_some()
            || case.validation_status.is_some()
            || case.validation_reason.is_some()
            || case.validation_resources.is_some())
    {
        return Err(CliError::new(format!(
            "non-kilometer case {} contains validation evidence",
            case.id
        )));
    }
    if case.validation_status.is_none()
        && (case.validation.is_some()
            || case.validation_reason.is_some()
            || case.validation_resources.is_some())
    {
        return Err(CliError::new(format!(
            "case {} has partial validation-attempt evidence",
            case.id
        )));
    }
    if case.validation_status.is_some() && case.validation_resources.is_none() {
        return Err(CliError::new(format!(
            "case {} validation status lacks resource evidence",
            case.id
        )));
    }
    if case.family.contains("bake")
        && case.status == CaseStatus::Passed
        && case.package_sha256.is_none()
    {
        return Err(CliError::new(format!(
            "passed bake case {} lacks a serialized package",
            case.id
        )));
    }
    if case.family.contains("bake")
        && case.package_sha256.is_some()
        && (case.probe_count.unwrap_or(0) == 0
            || case.path_data_bytes.unwrap_or(0) == 0
            || case.serialized_package_bytes.unwrap_or(0) == 0
            || case.bake_delivered.is_none())
    {
        return Err(CliError::new(format!(
            "passed bake case {} lacks nonzero evidence",
            case.id
        )));
    }
    if case.family.contains("bake") {
        if let Some(hash) = &case.package_sha256 {
            let metadata = serde_json::json!({
                "probe_count": case.probe_count,
                "path_data_size_bytes": case.path_data_bytes,
                "serialized_size_bytes": case.serialized_package_bytes,
                "content_sha256": hash,
                "bake_progress_callback_count": case.bake_delivered.as_ref()
                    .and_then(|value| value["metadata"]["bake_progress_callback_count"].as_u64()),
                "final_bake_progress_millionths": case.bake_delivered.as_ref()
                    .and_then(|value| value["metadata"]["final_bake_progress_millionths"].as_u64()),
            });
            let delivered = case.bake_delivered.as_ref().ok_or_else(|| {
                CliError::new(format!("bake case {} lacks delivered metadata", case.id))
            })?;
            if delivered["settings"] != case.requested
                || delivered["metadata"]["probe_count"] != metadata["probe_count"]
                || delivered["metadata"]["path_data_size_bytes"] != metadata["path_data_size_bytes"]
                || delivered["metadata"]["serialized_size_bytes"]
                    != metadata["serialized_size_bytes"]
                || delivered["metadata"]["content_sha256"] != metadata["content_sha256"]
                || delivered["metadata"]["bake_progress_callback_count"]
                    .as_u64()
                    .is_none_or(|value| value == 0)
                || delivered["metadata"]["final_bake_progress_millionths"] != 1_000_000
            {
                return Err(CliError::new(format!(
                    "bake case {} requested/delivered metadata mismatch",
                    case.id
                )));
            }
        } else if case.bake_delivered.is_some() {
            return Err(CliError::new(format!(
                "bake case {} has delivered metadata without a package",
                case.id
            )));
        }
    }
    let positive_bake = case.family.contains("bake")
        && case.requested["positive_cell"] == true
        && case.package_sha256.is_some();
    let viability_controls_status = positive_bake
        && (case.family != "kilometer_bake" || case.validation_status == Some(CaseStatus::Passed));
    if viability_controls_status
        && ((case.status == CaseStatus::Passed && !bake_within_viability_budgets(case))
            || (case.status == CaseStatus::QualityFailed && bake_within_viability_budgets(case)))
    {
        return Err(CliError::new(format!(
            "bake case {} status does not match recomputed viability budgets",
            case.id
        )));
    }
    if case.family == "kilometer_bake" && case.package_sha256.is_some() {
        let validation_status = case.validation_status.ok_or_else(|| {
            CliError::new(format!(
                "kilometer case {} lacks validation status",
                case.id
            ))
        })?;
        let validation_resources = case.validation_resources.as_ref().ok_or_else(|| {
            CliError::new(format!(
                "kilometer case {} lacks validation resources",
                case.id
            ))
        })?;
        verify_exit_status(
            validation_status,
            validation_resources,
            case.validation_reason.as_deref(),
            &format!("{} validation", case.id),
        )?;
        let positive = case.requested["positive_cell"] == true;
        let validated = case
            .validation
            .as_ref()
            .is_some_and(validated_positive_path);
        if let Some(validation) = &case.validation {
            let delivered = validation.delivered_arrival_degrees;
            let recomputed_delta =
                delivered.map(|value| angular_delta(value, ANALYTIC_AZIMUTH_DEGREES));
            let recomputed_analytic_passed =
                recomputed_delta.is_some_and(|value| value <= ANALYTIC_AZIMUTH_TOLERANCE_DEGREES);
            if !validation.analytic_arrival_degrees.is_finite()
                || validation.analytic_arrival_degrees != ANALYTIC_AZIMUTH_DEGREES
                || delivered.is_some_and(|value| !value.is_finite())
                || validation
                    .angular_error_degrees
                    .is_some_and(|value| !value.is_finite())
                || validation.angular_error_degrees != recomputed_delta
                || validation.analytic_arrival_passed != recomputed_analytic_passed
            {
                return Err(CliError::new(format!(
                    "kilometer case {} analytic arrival evidence is not recomputable",
                    case.id
                )));
            }
            let expected_validation_status = if validated {
                CaseStatus::Passed
            } else if !positive || !validation.path_output_nonzero {
                CaseStatus::NoPath
            } else {
                CaseStatus::QualityFailed
            };
            if validation_status != expected_validation_status {
                return Err(CliError::new(format!(
                    "kilometer case {} validation status does not match evidence",
                    case.id
                )));
            }
        } else if matches!(
            validation_status,
            CaseStatus::Passed | CaseStatus::NoPath | CaseStatus::QualityFailed
        ) {
            return Err(CliError::new(format!(
                "kilometer case {} validation status requires an evidence envelope",
                case.id
            )));
        }
        if !positive {
            let expected_cell_status = if validated {
                CaseStatus::QualityFailed
            } else {
                CaseStatus::NoPath
            };
            if case.status != expected_cell_status {
                return Err(CliError::new(
                    "designated 1000m negative status does not match validated reach evidence",
                ));
            }
        } else {
            let expected_cell_status = if validation_status == CaseStatus::Passed {
                if bake_within_viability_budgets(case) {
                    CaseStatus::Passed
                } else {
                    CaseStatus::QualityFailed
                }
            } else {
                validation_status
            };
            if case.status != expected_cell_status {
                return Err(CliError::new(format!(
                    "positive kilometer case {} status does not match validation and viability evidence",
                    case.id
                )));
            }
        }
        if let Some(validation) = &case.validation {
            let expected = validation_settings_for_case(case)?;
            if validation.requested_settings != expected
                || validation.delivered_settings != expected
                || !validation.path_output_finite
                || validation.occluded_validation_segments != 0
            {
                return Err(CliError::new(format!(
                    "kilometer case {} has invalid requested/delivered validation evidence",
                    case.id
                )));
            }
        }
    }
    if let Some(benchmark) = &case.benchmark {
        verify_benchmark(case, benchmark, mode, protocol)?;
    } else if case.family == "retained_runtime"
        && matches!(case.status, CaseStatus::Passed | CaseStatus::QualityFailed)
    {
        return Err(CliError::new(format!(
            "runtime case {} status requires retained raw benchmark evidence",
            case.id
        )));
    }
    Ok(())
}

fn verify_authority(
    root: &Path,
    authority: &AuthorityProvenance,
    mode: SweepMode,
    protocol: &Protocol,
) -> Result<()> {
    verify_authority_metadata(authority, mode, protocol)?;
    let dylib = safe_bundle_artifact(root, &authority.steam_audio_dylib_path)?;
    let engine = safe_bundle_artifact(root, &authority.engine_executable_path)?;
    let dylib_bytes = fs::read(dylib).map_err(io("read bundled authority SDK dylib"))?;
    let dylib_hash = sha256_hex(&dylib_bytes);
    if dylib_hash != PINNED_LIBPHONON_SHA256 {
        return Err(CliError::new(
            "sweep authority artifacts do not match their immutable checksums",
        ));
    }
    let engine_bytes = fs::read(engine).map_err(io("read bundled authority engine executable"))?;
    let engine_hash = sha256_hex(&engine_bytes);
    if engine_hash != authority.engine_executable_sha256 {
        return Err(CliError::new(
            "sweep authority artifacts do not match their immutable checksums",
        ));
    }
    let verifier_bytes =
        fs::read(std::env::current_exe().map_err(io("resolve verifier executable"))?)
            .map_err(io("read verifier executable"))?;
    verify_authority_artifact_hashes(
        authority,
        &dylib_hash,
        &engine_hash,
        &sha256_hex(&verifier_bytes),
        engine_bytes == verifier_bytes,
    )
}

fn verify_authority_metadata(
    authority: &AuthorityProvenance,
    mode: SweepMode,
    protocol: &Protocol,
) -> Result<()> {
    let expected_profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    if authority.engine_identity != provenance::ENGINE_IDENTITY
        || authority.source_state != "unborn_main_uncommitted_source"
        || authority.source_identity_sha256 != source_identity_sha256()?
        || authority.engine_executable_path != BUNDLED_ENGINE_PATH
        || authority.build_profile != expected_profile
        || authority.platform != provenance::platform()
        || authority.cpu_class != provenance::cpu_class()
        || authority.steam_audio_version != STEAM_AUDIO_VERSION
        || authority.steam_audio_upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT
        || authority.sample_rate_hz != 48_000
        || authority.canonical_plan_sha256 != hash_json(&canonical_plan_json(mode, protocol))?
        || authority.steam_audio_dylib_path != BUNDLED_LIBPHONON_PATH
        || authority.steam_audio_dylib_sha256 != PINNED_LIBPHONON_SHA256
    {
        return Err(CliError::new("invalid sweep authority provenance"));
    }
    Ok(())
}

fn verify_authority_artifact_hashes(
    authority: &AuthorityProvenance,
    dylib_hash: &str,
    engine_hash: &str,
    verifier_hash: &str,
    engine_matches_verifier: bool,
) -> Result<()> {
    if dylib_hash != PINNED_LIBPHONON_SHA256
        || engine_hash != authority.engine_executable_sha256
        || authority.engine_executable_sha256 != verifier_hash
        || !engine_matches_verifier
        || authority.engine_executable_sha256.len() != 64
        || !authority
            .engine_executable_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(CliError::new(
            "sweep authority artifacts do not match their immutable checksums",
        ));
    }
    Ok(())
}

fn safe_bundle_artifact(root: &Path, relative: &str) -> Result<PathBuf> {
    use std::path::Component;

    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CliError::new(format!(
            "invalid sweep authority artifact path {relative:?}"
        )));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(io("canonicalize sweep bundle root"))?;
    let canonical = root
        .join(path)
        .canonicalize()
        .map_err(io("canonicalize sweep authority artifact"))?;
    if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
        return Err(CliError::new(
            "sweep authority artifact resolves outside the report bundle",
        ));
    }
    Ok(canonical)
}

fn verify_exit_status(
    status: CaseStatus,
    resources: &Resources,
    reason: Option<&str>,
    label: &str,
) -> Result<()> {
    let consistent = match status {
        CaseStatus::ProjectedSkip => resources == &Resources::unrun(),
        CaseStatus::Timeout => resources.termination.as_deref() == Some("timeout"),
        CaseStatus::ResourceKilled => resources.termination.as_deref() == Some("resource_killed"),
        CaseStatus::Error => {
            resources.termination.as_deref() == Some("error")
                || (resources.termination.is_none()
                    && (resources.child_exit_code != Some(0)
                        || resources.child_signal.is_some()
                        || reason.is_some()))
        }
        CaseStatus::Passed | CaseStatus::NoPath | CaseStatus::QualityFailed => {
            resources.termination.is_none()
                && resources.child_exit_code == Some(0)
                && resources.child_signal.is_none()
        }
    };
    if !consistent
        || (status == CaseStatus::Passed && reason.is_some())
        || (status != CaseStatus::Passed && status != CaseStatus::ProjectedSkip && reason.is_none())
    {
        return Err(CliError::new(format!(
            "{label} status/exit/termination/reason fields are inconsistent"
        )));
    }
    Ok(())
}

fn verify_benchmark(
    case: &CaseReport,
    benchmark: &BenchmarkEvidence,
    _mode: SweepMode,
    protocol: &Protocol,
) -> Result<()> {
    let spec = runtime_plan()
        .into_iter()
        .find(|spec| spec.id == case.id)
        .ok_or_else(|| CliError::new(format!("unknown runtime case {}", case.id)))?;
    let expected_settings = settings_json(
        &render_request(
            World::Local,
            spec.spacing_m,
            spec.path_range_m,
            spec.block_size,
            spec.path_order,
            spec.direct,
            spec.reflection,
        )
        .simulation,
        spec.block_size,
    );
    if benchmark.requested_settings != expected_settings
        || benchmark.delivered_settings != expected_settings
        || case.runtime_delivered.as_ref() != Some(&expected_settings)
        || case.bake_delivered.is_some()
        || benchmark.loaded_probe_count != case.probe_count.unwrap_or(0)
        || benchmark.loaded_path_data_size_bytes != case.path_data_bytes.unwrap_or(0)
    {
        return Err(CliError::new(format!(
            "runtime case {} requested/delivered/loaded evidence mismatch",
            case.id
        )));
    }
    let stages = [
        (&benchmark.direct_simulation, protocol.standard_measured),
        (&benchmark.path_simulation, protocol.standard_measured),
        (
            &benchmark.reflection_simulation,
            protocol.reflection_measured,
        ),
        (
            &benchmark.direct_effect_binaural_apply,
            protocol.effect_measured,
        ),
        (&benchmark.path_effect_apply, protocol.effect_measured),
        (
            &benchmark.reflection_effect_decode_apply,
            protocol.effect_measured,
        ),
    ];
    for (stage, expected_n) in stages {
        if stage.raw_ns.len() != expected_n as usize
            || stage.derived.n != expected_n as usize
            || percentiles(&stage.raw_ns)? != stage.derived
        {
            return Err(CliError::new(format!(
                "runtime case {} has invalid raw N or percentile evidence",
                case.id
            )));
        }
    }
    let f = &benchmark.finite;
    if !(f.direct_simulation
        && f.path_simulation
        && f.reflection_simulation
        && f.direct_effect_binaural_apply
        && f.path_effect_apply
        && f.reflection_effect_decode_apply)
        || f.direct_simulation_samples_checked != protocol.standard_measured
        || f.path_simulation_samples_checked != protocol.standard_measured
        || f.reflection_simulation_samples_checked != protocol.reflection_measured
        || f.direct_effect_samples_checked != protocol.effect_measured
        || f.path_effect_samples_checked != protocol.effect_measured
        || f.reflection_effect_samples_checked != protocol.effect_measured
        || benchmark.retained_rendered_blocks != protocol.effect_warmups + protocol.effect_measured
    {
        return Err(CliError::new(format!(
            "runtime case {} finite counters/flags or retained blocks mismatch",
            case.id
        )));
    }
    let expected_budget = benchmark_budget(
        &benchmark.direct_simulation,
        &benchmark.path_simulation,
        &benchmark.reflection_simulation,
    );
    if benchmark.budget_result != expected_budget
        || (expected_budget == "hard_limit_exceeded" && case.status != CaseStatus::QualityFailed)
        || (expected_budget != "hard_limit_exceeded" && case.status == CaseStatus::QualityFailed)
    {
        return Err(CliError::new(format!(
            "runtime case {} budget result/status mismatch",
            case.id
        )));
    }
    Ok(())
}

fn benchmark_budget(direct: &Stage, path: &Stage, reflection: &Stage) -> &'static str {
    let hard = direct.derived.p99_ns > 16_700_000
        || path.derived.p99_ns > 66_700_000
        || reflection.derived.p99_ns > 200_000_000;
    let target = direct.derived.p99_ns > 8_000_000
        || path.derived.p99_ns > 20_000_000
        || reflection.derived.p99_ns > 100_000_000;
    if hard {
        "hard_limit_exceeded"
    } else if target {
        "target_exceeded"
    } else {
        "within_targets"
    }
}

fn validation_settings_for_case(case: &CaseReport) -> Result<serde_json::Value> {
    let spec = bake_plan()
        .into_iter()
        .find(|spec| spec.id == case.id)
        .ok_or_else(|| CliError::new(format!("unknown bake case {}", case.id)))?;
    Ok(settings_json(
        &render_request(
            spec.world,
            spec.spacing_m,
            spec.path_range_m,
            128,
            1,
            DirectVariant::Raycast,
            ReflectionVariant::Convolution,
        )
        .simulation,
        128,
    ))
}

fn validated_positive_path(validation: &ValidationEvidence) -> bool {
    let recomputed_delta = validation
        .delivered_arrival_degrees
        .filter(|value| value.is_finite())
        .map(|value| angular_delta(value, ANALYTIC_AZIMUTH_DEGREES));
    validation.fresh_process
        && validation.path_output_nonzero
        && validation.path_output_finite
        && validation.analytic_arrival_degrees == ANALYTIC_AZIMUTH_DEGREES
        && validation.analytic_arrival_passed
        && validation.angular_error_degrees == recomputed_delta
        && recomputed_delta.is_some_and(|value| value <= ANALYTIC_AZIMUTH_TOLERANCE_DEGREES)
        && validation.validation_segments > 0
        && validation.occluded_validation_segments == 0
}

fn bake_within_viability_budgets(case: &CaseReport) -> bool {
    case.resources
        .wall_time_ms
        .is_some_and(|value| value <= POSITIVE_BAKE_LIMIT_MS)
        && peak_rss(&case.resources).is_some_and(|value| value <= MAX_RSS_BUDGET)
        && case
            .path_data_bytes
            .is_some_and(|value| value <= PATH_DATA_BUDGET)
        && case
            .serialized_package_bytes
            .is_some_and(|value| value <= PACKAGE_BUDGET)
}

fn verify_case_artifacts(root: &Path, case: &CaseReport) -> Result<()> {
    let Some(expected_hash) = &case.package_sha256 else {
        return Ok(());
    };
    let bake_id = if case.family == "retained_runtime" {
        case.requested["bake_id"]
            .as_str()
            .ok_or_else(|| CliError::new(format!("runtime case {} lacks bake_id", case.id)))?
    } else {
        &case.id
    };
    let bake_dir = safe_case_dir(root, bake_id)?;
    let bytes = fs::read(bake_dir.join("probe-batch.bin"))
        .map_err(io("read self-contained sweep probe batch"))?;
    let wire: BakeWire = read_json(&bake_dir.join("bake.json"))?;
    if sha256_hex(&bytes) != *expected_hash
        || wire.metadata.content_sha256 != *expected_hash
        || Some(bytes.len() as u64) != case.serialized_package_bytes
        || Some(wire.metadata.serialized_size_bytes) != case.serialized_package_bytes
        || Some(wire.metadata.probe_count) != case.probe_count
        || Some(wire.metadata.path_data_size_bytes) != case.path_data_bytes
        || case.bake_delivered.as_ref().is_some_and(|value| {
            value["metadata"]["bake_progress_callback_count"]
                != wire.metadata.bake_progress_callback_count
                || value["metadata"]["final_bake_progress_millionths"]
                    != wire.metadata.final_bake_progress_millionths
        })
    {
        return Err(CliError::new(format!(
            "case {} is not bound to its self-contained bake artifacts",
            case.id
        )));
    }
    Ok(())
}

fn safe_case_dir(root: &Path, id: &str) -> Result<PathBuf> {
    use std::path::Component;

    let mut components = Path::new(id).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    {
        return Err(CliError::new(format!(
            "invalid non-component-safe sweep case id {id:?}"
        )));
    }
    let cases_root = root.join("cases");
    let candidate = cases_root.join(id);
    let canonical_root = root
        .canonicalize()
        .map_err(io("canonicalize sweep bundle root"))?;
    let canonical = candidate
        .canonicalize()
        .map_err(io("canonicalize sweep case directory"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(CliError::new(format!(
            "sweep case {id:?} resolves outside the report bundle"
        )));
    }
    Ok(canonical)
}

fn verify_resources(label: &str, resources: &Resources) -> Result<()> {
    let actual_peak = resources.sampled_rss_bytes.iter().copied().max();
    if resources.peak_sampled_rss_bytes != actual_peak {
        return Err(CliError::new(format!(
            "{label} peak_sampled_rss_bytes does not equal the maximum raw RSS sample"
        )));
    }
    if resources.wall_time_ms.is_none()
        && (resources.child_exit_code.is_some()
            || resources.child_signal.is_some()
            || resources.time_l_max_rss_bytes.is_some()
            || !resources.sampled_rss_bytes.is_empty()
            || resources.temp_directory_peak_bytes.is_some()
            || resources.termination.is_some())
    {
        return Err(CliError::new(format!(
            "{label} unrun resources contain observed values"
        )));
    }
    Ok(())
}

fn formal_decision(mode: SweepMode, cases: &[CaseReport], date: &str) -> Decision {
    if mode == SweepMode::Sampled {
        return Decision {
            applicability: "provisional_not_applicable".into(),
            kernel_reach: None,
            monolithic_economics: None,
            decision: "sampled_sweep_cannot_emit_phase_a_branch".into(),
            named_post_mvp_phase: None,
            decision_required_by: None,
        };
    }
    let reach: Vec<_> = cases
        .iter()
        .filter(|case| {
            case.family == "kilometer_bake"
                && matches!(case.status, CaseStatus::Passed | CaseStatus::QualityFailed)
                && case.requested["positive_cell"] == true
                && case.requested["path_range_m"]
                    .as_f64()
                    .is_some_and(|value| value >= 1_000.0)
                && case
                    .validation
                    .as_ref()
                    .is_some_and(validated_positive_path)
        })
        .collect();
    let kernel_reach = !reach.is_empty();
    let economics = reach.iter().any(|case| bake_within_viability_budgets(case));
    if !kernel_reach {
        Decision {
            applicability: "final_full_protocol".into(),
            kernel_reach: Some(kernel_reach),
            monolithic_economics: Some(false),
            decision: "stop_and_replan_narrow_outdoor_backend_before_phase_c".into(),
            named_post_mvp_phase: None,
            decision_required_by: None,
        }
    } else if economics {
        Decision {
            applicability: "final_full_protocol".into(),
            kernel_reach: Some(kernel_reach),
            monolithic_economics: Some(true),
            decision: "schedule_willis_tower_qualification".into(),
            named_post_mvp_phase: Some("post_mvp_kilometer_approach_qualification".into()),
            decision_required_by: None,
        }
    } else {
        Decision {
            applicability: "final_full_protocol".into(),
            kernel_reach: Some(kernel_reach),
            monolithic_economics: Some(false),
            decision: "explicit_monolithic_backend_decision_required_before_phase_c".into(),
            named_post_mvp_phase: None,
            decision_required_by: Some(date.into()),
        }
    }
}

fn projected_bake(spec: &BakeSpec, reason: &str) -> Result<CaseReport> {
    let requested = bake_requested(spec);
    Ok(CaseReport {
        id: spec.id.clone(),
        family: bake_family(spec).into(),
        input_hash: hash_json(&world_input(spec.world))?,
        configuration_hash: hash_json(&requested)?,
        status: CaseStatus::ProjectedSkip,
        reason: Some(reason.into()),
        requested,
        bake_delivered: None,
        runtime_delivered: None,
        resources: Resources::unrun(),
        validation_status: None,
        validation_reason: None,
        validation_resources: None,
        probe_count: None,
        path_data_bytes: None,
        serialized_package_bytes: None,
        package_sha256: None,
        benchmark: None,
        validation: None,
    })
}

fn projected_runtime(spec: &RuntimeSpec, protocol: &Protocol, reason: &str) -> Result<CaseReport> {
    let requested = runtime_requested(spec, protocol);
    Ok(CaseReport {
        id: spec.id.clone(),
        family: "retained_runtime".into(),
        input_hash: hash_json(&world_input(World::Local))?,
        configuration_hash: hash_json(&requested)?,
        status: CaseStatus::ProjectedSkip,
        reason: Some(reason.into()),
        requested,
        bake_delivered: None,
        runtime_delivered: None,
        resources: Resources::unrun(),
        validation_status: None,
        validation_reason: None,
        validation_resources: None,
        probe_count: None,
        path_data_bytes: None,
        serialized_package_bytes: None,
        package_sha256: None,
        benchmark: None,
        validation: None,
    })
}

fn bake_requested(spec: &BakeSpec) -> serde_json::Value {
    let request = bake_request(spec.world, spec.spacing_m, spec.path_range_m);
    serde_json::json!({
        "world": world_name(spec.world),
        "probe_spacing_m": spec.spacing_m,
        "path_range_m": spec.path_range_m,
        "probe_min_enu_m": [request.probes.min_enu_m.x, request.probes.min_enu_m.y, request.probes.min_enu_m.z],
        "probe_max_enu_m": [request.probes.max_enu_m.x, request.probes.max_enu_m.y, request.probes.max_enu_m.z],
        "probe_height_above_floor_m": request.probes.height_above_floor_m,
        "visibility_samples": request.pathing.num_visibility_samples,
        "visibility_radius_m": request.pathing.probe_visibility_radius_m,
        "visibility_threshold": request.pathing.visibility_threshold,
        "visibility_range_m": request.pathing.visibility_range_m,
        "threads": request.pathing.num_threads,
        "predicted_probes": spec.predicted_probes,
        "predicted_probe_pairs": spec.predicted_pairs,
        "positive_cell": spec.positive,
    })
}

fn runtime_requested(spec: &RuntimeSpec, protocol: &Protocol) -> serde_json::Value {
    let render = render_request(
        World::Local,
        spec.spacing_m,
        spec.path_range_m,
        spec.block_size,
        spec.path_order,
        spec.direct,
        spec.reflection,
    );
    serde_json::json!({
        "bake_id": spec.bake_id,
        "simulation": settings_json(&render.simulation, spec.block_size),
        "iterations": {
            "simulation_warmup": protocol.standard_warmups,
            "simulation_measured": protocol.standard_measured,
            "reflection_warmup": protocol.reflection_warmups,
            "reflection_measured": protocol.reflection_measured,
            "effect_warmup": protocol.effect_warmups,
            "effect_measured": protocol.effect_measured,
        }
    })
}

fn world_input(world: World) -> serde_json::Value {
    match world {
        World::Local => serde_json::json!({
            "geometry": "accepted_convex_corner_v1",
            "vertices_enu_m": [[0,0,0],[10,0,0],[10,0,6],[0,0,6],[0,10,0],[0,10,6],[-9,-9,0],[9,-9,0],[9,9,0],[-9,9,0]],
            "triangles": [[0,1,2],[0,2,3],[2,1,0],[3,2,0],[0,4,5],[0,5,3],[5,4,0],[3,5,0],[6,7,8],[6,8,9]],
            "material_indices": [0,0,0,0,0,0,0,0,0,0],
            "materials": [{"absorption":[0.03,0.05,0.07],"scattering":0.1,"transmission":[0.0,0.0,0.0]}],
            "source_enu_m": [-4, 6, 1.5],
            "listener_enu_m": [6, -4, 1.5],
        }),
        World::Kilometer => serde_json::json!({
            "geometry": "controlled_kilometer_convex_corner_v1",
            "vertices_enu_m": [[0,0,0],[700,0,0],[700,0,100],[0,0,100],[0,700,0],[0,700,100],[-470,-470,0],[680,-470,0],[680,680,0],[-470,680,0]],
            "triangles": [[0,1,2],[0,2,3],[2,1,0],[3,2,0],[0,4,5],[0,5,3],[5,4,0],[3,5,0],[6,7,8],[6,8,9],[8,7,6],[9,8,6]],
            "material_indices": [0,0,0,0,0,0,0,0,1,1,1,1],
            "materials": [
                {"absorption":[0.03,0.05,0.07],"scattering":0.1,"transmission":[0.0,0.0,0.0]},
                {"absorption":[0.05,0.07,0.08],"scattering":0.05,"transmission":[0.0,0.0,0.0]}
            ],
            "source_enu_m": [-400, 600, 1.5],
            "listener_enu_m": [600, -400, 1.5],
            "direct_distance_m": 1414.214,
            "route_via_origin_m": 1442.221,
            "analytic_arrival_degrees": ANALYTIC_AZIMUTH_DEGREES,
        }),
    }
}

fn settings_json(config: &S3SimulationConfig, block: i32) -> serde_json::Value {
    let direct = match config.direct_occlusion {
        DirectOcclusionMode::Raycast => serde_json::json!({"mode":"raycast"}),
        DirectOcclusionMode::Volumetric {
            radius_m,
            sample_count,
        } => serde_json::json!({
            "mode":"volumetric",
            "radius_m":radius_m,
            "sample_count":sample_count
        }),
    };
    serde_json::json!({
        "direct": direct,
        "max_occlusion_samples": config.max_occlusion_samples,
        "reflection": reflection_type_name(config.reflection_effect.effect_type),
        "reflection_duration_s": config.reflection_duration_s,
        "reflection_order": config.reflection_order,
        "reflection_rays": config.reflection_rays,
        "diffuse_samples": config.diffuse_samples,
        "reflection_bounces": config.reflection_bounces,
        "simulation_threads": config.simulation_threads,
        "ray_batch_size": config.ray_batch_size,
        "hybrid_transition_time_s": config.reflection_effect.hybrid_transition_time_s,
        "hybrid_overlap_percent": config.reflection_effect.hybrid_overlap_percent,
        "path_order": config.pathing_order,
        "pathing_visibility_samples": config.pathing_visibility_samples,
        "pathing_visibility_radius_m": config.pathing_visibility_radius_m,
        "pathing_visibility_threshold": config.pathing_visibility_threshold,
        "pathing_visibility_range_m": config.pathing_visibility_range_m,
        "validate_paths": config.validate_paths,
        "find_alternate_paths": config.find_alternate_paths,
        "trace_path_validation": config.trace_path_validation,
        "sample_rate_hz": 48_000,
        "block_size": block,
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).map_err(io("read JSON artifact"))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| CliError::new(format!("parse {}: {error}", path.display())))
}

fn peak_rss(resources: &Resources) -> Option<u64> {
    resources
        .time_l_max_rss_bytes
        .into_iter()
        .chain(resources.peak_sampled_rss_bytes)
        .max()
}

fn hash_json(value: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| CliError::new(format!("serialize hash input: {error}")))?;
    Ok(sha256_hex(&bytes))
}

fn source_identity_sha256() -> Result<String> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for name in ["Cargo.toml", "Cargo.lock"] {
        let path = workspace.join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    for name in ["crates", "tools"] {
        collect_source_identity_files(&workspace.join(name), &mut files)?;
    }
    files.sort();
    let mut identity = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&workspace)
            .map_err(|_| CliError::new("source identity path escaped workspace"))?;
        let bytes = fs::read(&path).map_err(io("read source identity input"))?;
        let name = relative.to_string_lossy();
        identity.extend_from_slice(&(name.len() as u64).to_le_bytes());
        identity.extend_from_slice(name.as_bytes());
        identity.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        identity.extend_from_slice(&bytes);
    }
    Ok(sha256_hex(&identity))
}

fn collect_source_identity_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(root)
        .map_err(io("read source identity directory"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(io("read source identity entry"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(io("inspect source identity entry"))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_source_identity_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs")
            || path.file_name().is_some_and(|name| {
                name == "Cargo.toml" || name == "Cargo.lock" || name == "build.rs"
            })
        {
            files.push(path);
        }
    }
    Ok(())
}

fn io(context: &'static str) -> impl FnOnce(std::io::Error) -> CliError {
    move |error| CliError::new(format!("{context}: {error}"))
}

fn world_name(value: World) -> &'static str {
    match value {
        World::Local => "local",
        World::Kilometer => "kilometer",
    }
}

fn bake_family(spec: &BakeSpec) -> &'static str {
    match spec.world {
        World::Local => "local_bake",
        World::Kilometer => "kilometer_bake",
    }
}

fn parse_world(value: &str) -> Result<World> {
    match value {
        "local" => Ok(World::Local),
        "kilometer" => Ok(World::Kilometer),
        _ => Err(CliError::new("invalid internal world")),
    }
}

fn direct_name(value: DirectVariant) -> &'static str {
    match value {
        DirectVariant::Raycast => "raycast",
        DirectVariant::Volumetric05 => "volumetric-0.5m-16",
        DirectVariant::Volumetric10 => "volumetric-1.0m-64",
    }
}

fn parse_direct(value: &str) -> Result<DirectVariant> {
    match value {
        "raycast" => Ok(DirectVariant::Raycast),
        "volumetric-0.5m-16" => Ok(DirectVariant::Volumetric05),
        "volumetric-1.0m-64" => Ok(DirectVariant::Volumetric10),
        _ => Err(CliError::new("invalid internal direct variant")),
    }
}

fn reflection_name(value: ReflectionVariant) -> &'static str {
    match value {
        ReflectionVariant::Convolution => "convolution",
        ReflectionVariant::Hybrid => "hybrid",
        ReflectionVariant::Parametric => "parametric",
    }
}

fn parse_reflection(value: &str) -> Result<ReflectionVariant> {
    match value {
        "convolution" => Ok(ReflectionVariant::Convolution),
        "hybrid" => Ok(ReflectionVariant::Hybrid),
        "parametric" => Ok(ReflectionVariant::Parametric),
        _ => Err(CliError::new("invalid internal reflection variant")),
    }
}

fn reflection_type_name(value: ReflectionEffectType) -> &'static str {
    match value {
        ReflectionEffectType::Convolution => "convolution",
        ReflectionEffectType::Hybrid => "hybrid",
        ReflectionEffectType::Parametric => "parametric",
        ReflectionEffectType::TrueAudioNext => "true_audio_next",
    }
}

fn compact(value: f32) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i32)
    } else {
        value.to_string().replace('.', "_")
    }
}

fn number<T: std::str::FromStr>(value: &str, name: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| CliError::new(format!("invalid internal {name}")))
}

fn angular_delta(a: f64, b: f64) -> f64 {
    ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fightbox-sweep-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn matrix_cardinalities_are_exact_and_not_naive_cartesian() {
        let bakes = bake_plan();
        assert_eq!(bakes.len(), 24);
        assert_eq!(bakes.iter().filter(|b| b.world == World::Local).count(), 12);
        assert_eq!(
            bakes
                .iter()
                .filter(|b| b.world == World::Kilometer && [50.0, 100.0].contains(&b.spacing_m))
                .count(),
            6
        );
        let runtime = runtime_plan();
        assert_eq!(
            runtime
                .iter()
                .filter(|r| r.id.starts_with("runtime-order"))
                .count(),
            48
        );
        assert_eq!(
            runtime
                .iter()
                .filter(|r| r.id.starts_with("runtime-cross"))
                .count(),
            36
        );
        assert_eq!(
            runtime
                .iter()
                .filter(|r| r.id.starts_with("runtime-guard"))
                .count(),
            8
        );
        assert_eq!(runtime.len(), 92);
        assert_ne!(runtime.len(), 1_728);
        let sampled = Protocol::for_mode(SweepMode::Sampled);
        assert_eq!(canonical_cases(SweepMode::Sampled, &sampled).len(), 116);
        assert_eq!(
            bakes
                .iter()
                .filter(|spec| bake_selected(SweepMode::Sampled, spec))
                .count(),
            4
        );
        assert_eq!(
            runtime
                .iter()
                .filter(|spec| runtime_selected(SweepMode::Sampled, spec))
                .count(),
            4
        );
        for spec in bakes.iter().filter(|spec| spec.world == World::Kilometer) {
            if spec.spacing_m == 25.0 {
                assert!(optional_25m_permitted(spec));
                assert!(bake_selected(SweepMode::Full, spec));
            }
            if spec.spacing_m == 12.5 {
                assert!(!bake_selected(SweepMode::Full, spec));
            }
        }
    }

    #[test]
    fn nearest_rank_percentiles_report_n() {
        let value = percentiles(&[100, 1, 5, 3]).unwrap();
        assert_eq!(value.n, 4);
        assert_eq!(
            (value.p50_ns, value.p95_ns, value.p99_ns, value.max_ns),
            (3, 100, 100, 100)
        );
        assert!(percentiles(&[]).is_err());
    }

    #[test]
    fn canonical_raycast_and_volumetric_variants_are_exact() {
        let ray = render_request(
            World::Local,
            2.0,
            100.0,
            128,
            2,
            DirectVariant::Raycast,
            ReflectionVariant::Convolution,
        );
        assert_eq!(
            ray.simulation.direct_occlusion,
            DirectOcclusionMode::Raycast
        );
        assert_eq!(ray.simulation.max_occlusion_samples, 64);
        for variant in [DirectVariant::Volumetric05, DirectVariant::Volumetric10] {
            let request = render_request(
                World::Local,
                2.0,
                100.0,
                128,
                2,
                variant,
                ReflectionVariant::Convolution,
            );
            match request.simulation.direct_occlusion {
                DirectOcclusionMode::Volumetric {
                    radius_m,
                    sample_count,
                } => {
                    assert!(radius_m > 0.0);
                    assert!(matches!((radius_m, sample_count), (0.5, 16) | (1.0, 64)));
                }
                _ => panic!("expected volumetric"),
            }
        }
    }

    #[test]
    fn projected_cells_have_null_metrics() {
        let case = projected_bake(&bake_plan()[0], "test").unwrap();
        assert_eq!(case.status, CaseStatus::ProjectedSkip);
        assert!(case.probe_count.is_none());
        assert!(case.path_data_bytes.is_none());
        assert!(case.benchmark.is_none());
        verify_case(
            &case,
            SweepMode::Sampled,
            &Protocol::for_mode(SweepMode::Sampled),
        )
        .unwrap();
    }

    #[test]
    fn resource_thresholds_and_time_parser_are_exact() {
        assert_eq!(LIVE_RSS_KILL, 8 * GIB);
        assert_eq!(TEMP_CAP, GIB);
        assert_eq!(PROBE_CAP, 4_096);
        assert_eq!(PAIR_CAP, 16_777_216);
        assert_eq!(
            parse_time_l_max_rss("  123456  maximum resident set size"),
            Some(123_456)
        );
    }

    #[test]
    fn timeout_and_resource_kill_override_child_status() {
        let envelope = ChildEnvelope {
            status: CaseStatus::Passed,
            reason: None,
            bake: None,
            benchmark: None,
            validation: None,
            delivered: None,
        };
        for (termination, expected) in [
            ("timeout", CaseStatus::Timeout),
            ("resource_killed", CaseStatus::ResourceKilled),
        ] {
            let run = RunOutcome {
                resources: Resources {
                    termination: Some(termination.into()),
                    ..Resources::unrun()
                },
                reason: None,
            };
            assert_eq!(classify(&run, Some(&envelope)), expected);
        }
    }

    fn reach_case() -> CaseReport {
        let spec = bake_plan()
            .into_iter()
            .find(|spec| spec.id == "km-s100-r1750")
            .unwrap();
        let mut case = projected_bake(&spec, "test").unwrap();
        case.status = CaseStatus::Passed;
        case.reason = None;
        case.probe_count = Some(144);
        case.path_data_bytes = Some(1);
        case.serialized_package_bytes = Some(1);
        case.package_sha256 = Some("a".repeat(64));
        case.resources = successful_resources();
        case.bake_delivered = Some(serde_json::json!({
            "settings": case.requested.clone(),
            "metadata": {
                "probe_count": 144,
                "path_data_size_bytes": 1,
                "serialized_size_bytes": 1,
                "content_sha256": "a".repeat(64),
                "bake_progress_callback_count": 1,
                "final_bake_progress_millionths": 1_000_000,
            }
        }));
        let validation_settings = validation_settings_for_case(&case).unwrap();
        case.validation = Some(ValidationEvidence {
            fresh_process: true,
            path_output_nonzero: true,
            path_output_finite: true,
            analytic_arrival_degrees: ANALYTIC_AZIMUTH_DEGREES,
            delivered_arrival_degrees: Some(ANALYTIC_AZIMUTH_DEGREES),
            angular_error_degrees: Some(0.0),
            analytic_arrival_passed: true,
            validation_segments: 1,
            occluded_validation_segments: 0,
            requested_settings: validation_settings.clone(),
            delivered_settings: validation_settings,
        });
        case.validation_status = Some(CaseStatus::Passed);
        case.validation_resources = Some(Resources {
            wall_time_ms: Some(1),
            child_exit_code: Some(0),
            child_signal: None,
            time_l_max_rss_bytes: Some(1),
            sampled_rss_bytes: vec![1, 2],
            peak_sampled_rss_bytes: Some(2),
            temp_directory_peak_bytes: Some(1),
            termination: None,
        });
        case
    }

    #[test]
    fn formal_decision_has_all_three_branches() {
        assert_eq!(
            formal_decision(SweepMode::Full, &[], "2026-08-05").decision,
            "stop_and_replan_narrow_outdoor_backend_before_phase_c"
        );
        let mut case = reach_case();
        assert_eq!(
            formal_decision(SweepMode::Full, &[case.clone()], "2026-08-05").decision,
            "schedule_willis_tower_qualification"
        );
        case.resources.time_l_max_rss_bytes = Some(MAX_RSS_BUDGET + 1);
        let decision = formal_decision(SweepMode::Full, &[case], "2026-08-05");
        assert_eq!(
            decision.decision,
            "explicit_monolithic_backend_decision_required_before_phase_c"
        );
        assert_eq!(decision.decision_required_by.as_deref(), Some("2026-08-05"));
    }

    #[test]
    fn kilometer_analytic_and_status_mutations_are_rejected() {
        let protocol = Protocol::for_mode(SweepMode::Sampled);
        let baseline = reach_case();
        verify_case(&baseline, SweepMode::Sampled, &protocol).unwrap();
        for mutation in [
            "analytic",
            "delivered",
            "boolean",
            "segments",
            "validation_status",
        ] {
            let mut case = baseline.clone();
            let validation = case.validation.as_mut().unwrap();
            match mutation {
                "analytic" => validation.analytic_arrival_degrees = 100.0,
                "delivered" => {
                    validation.delivered_arrival_degrees = Some(100.0);
                    validation.angular_error_degrees =
                        Some(angular_delta(100.0, ANALYTIC_AZIMUTH_DEGREES));
                    validation.analytic_arrival_passed = true;
                }
                "boolean" => validation.analytic_arrival_passed = false,
                "segments" => validation.validation_segments = 0,
                "validation_status" => {
                    case.status = CaseStatus::NoPath;
                    case.reason = Some("forged validation status".into());
                    case.validation_status = Some(CaseStatus::NoPath);
                    case.validation_reason = Some("forged validation status".into());
                }
                _ => unreachable!(),
            }
            case.configuration_hash = hash_json(&case.requested).unwrap();
            assert!(
                verify_case(&case, SweepMode::Sampled, &protocol).is_err(),
                "mutation {mutation}"
            );
        }
    }

    #[test]
    fn bake_viability_status_is_recomputed_from_every_budget() {
        let protocol = Protocol::for_mode(SweepMode::Sampled);
        let baseline = reach_case();
        for mutation in ["wall", "rss", "path", "package"] {
            let mut case = baseline.clone();
            match mutation {
                "wall" => case.resources.wall_time_ms = Some(POSITIVE_BAKE_LIMIT_MS + 1),
                "rss" => case.resources.time_l_max_rss_bytes = Some(MAX_RSS_BUDGET + 1),
                "path" => {
                    case.path_data_bytes = Some(PATH_DATA_BUDGET + 1);
                    case.bake_delivered.as_mut().unwrap()["metadata"]["path_data_size_bytes"] =
                        (PATH_DATA_BUDGET + 1).into();
                }
                "package" => {
                    case.serialized_package_bytes = Some(PACKAGE_BUDGET + 1);
                    case.bake_delivered.as_mut().unwrap()["metadata"]["serialized_size_bytes"] =
                        (PACKAGE_BUDGET + 1).into();
                }
                _ => unreachable!(),
            }
            assert!(
                verify_case(&case, SweepMode::Sampled, &protocol).is_err(),
                "mutation {mutation}"
            );
            case.status = CaseStatus::QualityFailed;
            case.reason = Some("positive bake exceeded one or more viability budgets".into());
            verify_case(&case, SweepMode::Sampled, &protocol).unwrap();
        }
        let mut forged_failure = baseline;
        forged_failure.status = CaseStatus::QualityFailed;
        forged_failure.reason = Some("positive bake exceeded one or more viability budgets".into());
        assert!(verify_case(&forged_failure, SweepMode::Sampled, &protocol).is_err());
    }

    #[test]
    fn missing_bake_and_runtime_envelopes_are_explicit_reportable_errors() {
        let run = RunOutcome {
            resources: successful_resources(),
            reason: None,
        };
        for kind in ["bake", "runtime"] {
            let (status, reason) = child_status_and_reason(&run, None, kind);
            let expected = format!("{kind} child produced no readable result envelope");
            assert_eq!(status, CaseStatus::Error);
            assert_eq!(reason.as_deref(), Some(expected.as_str()));
            verify_exit_status(status, &run.resources, reason.as_deref(), kind).unwrap();
            assert_eq!(run.resources.wall_time_ms, Some(1));
            assert_eq!(run.resources.peak_sampled_rss_bytes, Some(7));
        }
    }

    #[test]
    fn absent_bake_envelope_discards_real_order_partial_and_atomic_artifacts() {
        let root = temp_test_root("absent-envelope-partials");
        let case_dir = root.join("cases/local-s2-r100");
        fs::create_dir_all(&case_dir).unwrap();
        fs::write(case_dir.join("time-bake.log"), b"completed supervisor log").unwrap();
        fs::write(case_dir.join("probe-batch.bin"), b"partial probe bytes").unwrap();
        fs::write(case_dir.join("bake.json"), b"{\"partial\":true}").unwrap();
        fs::write(
            case_dir.join(".probe-batch.bin.tmp.interrupted"),
            b"interrupted probe temp",
        )
        .unwrap();
        fs::write(
            case_dir.join(".bake.json.tmp.interrupted"),
            b"interrupted bake temp",
        )
        .unwrap();
        fs::write(
            case_dir.join(".child.json.tmp.interrupted"),
            b"interrupted envelope temp",
        )
        .unwrap();
        discard_partial_bake_artifacts(&case_dir).unwrap();
        assert!(!case_dir.join("probe-batch.bin").exists());
        assert!(!case_dir.join("bake.json").exists());
        assert!(fs::read_dir(&case_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        }));
        assert_eq!(
            fs::read(case_dir.join("time-bake.log")).unwrap(),
            b"completed supervisor log"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn projected_metrics_mutation_is_rejected() {
        let mut case = projected_bake(&bake_plan()[0], "test").unwrap();
        case.probe_count = Some(1);
        assert!(
            verify_case(
                &case,
                SweepMode::Sampled,
                &Protocol::for_mode(SweepMode::Sampled)
            )
            .is_err()
        );
    }

    #[test]
    fn incorrect_sampled_rss_peak_mutation_is_rejected() {
        let mut case = projected_bake(&bake_plan()[0], "test").unwrap();
        case.resources = Resources {
            wall_time_ms: Some(1),
            child_exit_code: Some(0),
            child_signal: None,
            time_l_max_rss_bytes: Some(3),
            sampled_rss_bytes: vec![1, 7, 2],
            peak_sampled_rss_bytes: Some(2),
            temp_directory_peak_bytes: Some(1),
            termination: None,
        };
        assert!(
            verify_case(
                &case,
                SweepMode::Sampled,
                &Protocol::for_mode(SweepMode::Sampled)
            )
            .is_err()
        );
    }

    fn synthetic_report_without_artifacts(mode: SweepMode) -> Report {
        let protocol = Protocol::for_mode(mode);
        let mut cases = Vec::new();
        for spec in bake_plan() {
            let selected = bake_selected(mode, &spec);
            let mut case = projected_bake(&spec, "not selected by synthetic plan").unwrap();
            if selected {
                case.status = CaseStatus::Error;
                case.reason = Some("synthetic attempted child error".into());
                case.resources = attempted_error_resources();
            }
            cases.push(case);
        }
        for spec in runtime_plan() {
            let selected = runtime_selected(mode, &spec);
            let mut case =
                projected_runtime(&spec, &protocol, "not selected by synthetic plan").unwrap();
            if selected {
                case.status = CaseStatus::Error;
                case.reason = Some("synthetic attempted child error".into());
                case.resources = attempted_error_resources();
            }
            cases.push(case);
        }
        Report {
            schema_version: SCHEMA.into(),
            mode,
            generated_unix_seconds: 0,
            authority: synthetic_authority(mode, &protocol, "0".repeat(64), "a".repeat(64)),
            protocol,
            decision: formal_decision(mode, &cases, "2026-08-05"),
            cases,
            claims: Vec::new(),
            non_claims: Vec::new(),
        }
    }

    fn synthetic_report(root: &Path, mode: SweepMode) -> Report {
        let mut report = synthetic_report_without_artifacts(mode);
        for case in report
            .cases
            .iter()
            .filter(|case| case.status != CaseStatus::ProjectedSkip)
        {
            let dir = root.join("cases").join(&case.id);
            fs::create_dir_all(&dir).unwrap();
            let kind = if case.family == "retained_runtime" {
                "benchmark"
            } else {
                "bake"
            };
            fs::write(dir.join(format!("time-{kind}.log")), b"synthetic time").unwrap();
        }
        fs::create_dir_all(root.join("authority")).unwrap();
        let sdk_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(".cache/steam-audio/steamaudio-4.8.1/steamaudio");
        let sdk = SdkBinary::detect_from(Some(sdk_root.as_os_str()));
        let sdk_path = sdk.dylib_path.unwrap();
        let dylib_bytes = fs::read(&sdk_path).unwrap();
        assert_eq!(sha256_hex(&dylib_bytes), PINNED_LIBPHONON_SHA256);
        fs::hard_link(&sdk_path, root.join(BUNDLED_LIBPHONON_PATH)).unwrap();
        let executable = std::env::current_exe().unwrap();
        let executable_bytes = fs::read(&executable).unwrap();
        fs::hard_link(executable, root.join(BUNDLED_ENGINE_PATH)).unwrap();
        report.authority = synthetic_authority(
            mode,
            &report.protocol,
            source_identity_sha256().unwrap(),
            sha256_hex(&executable_bytes),
        );
        report
    }

    fn synthetic_authority(
        mode: SweepMode,
        protocol: &Protocol,
        source_identity_sha256: String,
        engine_executable_sha256: String,
    ) -> AuthorityProvenance {
        AuthorityProvenance {
            engine_identity: provenance::ENGINE_IDENTITY.into(),
            source_state: "unborn_main_uncommitted_source".into(),
            source_identity_sha256,
            engine_executable_path: BUNDLED_ENGINE_PATH.into(),
            engine_executable_sha256,
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .into(),
            platform: provenance::platform().into(),
            cpu_class: provenance::cpu_class().into(),
            steam_audio_version: STEAM_AUDIO_VERSION.into(),
            steam_audio_upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT.into(),
            steam_audio_dylib_path: BUNDLED_LIBPHONON_PATH.into(),
            steam_audio_dylib_sha256: PINNED_LIBPHONON_SHA256.into(),
            sample_rate_hz: 48_000,
            canonical_plan_sha256: hash_json(&canonical_plan_json(mode, protocol)).unwrap(),
        }
    }

    fn attempted_error_resources() -> Resources {
        Resources {
            wall_time_ms: Some(1),
            child_exit_code: Some(1),
            child_signal: None,
            time_l_max_rss_bytes: Some(7),
            sampled_rss_bytes: vec![7],
            peak_sampled_rss_bytes: Some(7),
            temp_directory_peak_bytes: Some(1),
            termination: None,
        }
    }

    fn successful_resources() -> Resources {
        Resources {
            wall_time_ms: Some(1),
            child_exit_code: Some(0),
            child_signal: None,
            time_l_max_rss_bytes: Some(7),
            sampled_rss_bytes: vec![7],
            peak_sampled_rss_bytes: Some(7),
            temp_directory_peak_bytes: Some(1),
            termination: None,
        }
    }

    fn benchmark_case() -> (CaseReport, Protocol) {
        let protocol = Protocol::for_mode(SweepMode::Sampled);
        let spec = runtime_plan()
            .into_iter()
            .find(|spec| spec.id == "runtime-cross-draycast-xconvolution-b128")
            .unwrap();
        let mut case = projected_runtime(&spec, &protocol, "test").unwrap();
        case.status = CaseStatus::Passed;
        case.reason = None;
        case.resources = successful_resources();
        case.probe_count = Some(81);
        case.path_data_bytes = Some(26_332);
        case.serialized_package_bytes = Some(28_892);
        case.package_sha256 = Some("a".repeat(64));
        let settings = settings_json(
            &render_request(
                World::Local,
                spec.spacing_m,
                spec.path_range_m,
                spec.block_size,
                spec.path_order,
                spec.direct,
                spec.reflection,
            )
            .simulation,
            spec.block_size,
        );
        case.runtime_delivered = Some(settings.clone());
        let standard = Stage {
            raw_ns: vec![1, 2, 3, 4],
            derived: percentiles(&[1, 2, 3, 4]).unwrap(),
        };
        let reflection = Stage {
            raw_ns: vec![5, 6],
            derived: percentiles(&[5, 6]).unwrap(),
        };
        case.benchmark = Some(BenchmarkEvidence {
            loaded_probe_count: 81,
            loaded_path_data_size_bytes: 26_332,
            retained_rendered_blocks: 5,
            requested_settings: settings.clone(),
            delivered_settings: settings,
            direct_simulation: standard.clone(),
            path_simulation: standard.clone(),
            reflection_simulation: reflection,
            direct_effect_binaural_apply: standard.clone(),
            path_effect_apply: standard.clone(),
            reflection_effect_decode_apply: standard,
            finite: FiniteEvidence {
                direct_simulation: true,
                path_simulation: true,
                reflection_simulation: true,
                direct_effect_binaural_apply: true,
                path_effect_apply: true,
                reflection_effect_decode_apply: true,
                direct_simulation_samples_checked: 4,
                path_simulation_samples_checked: 4,
                reflection_simulation_samples_checked: 2,
                direct_effect_samples_checked: 4,
                path_effect_samples_checked: 4,
                reflection_effect_samples_checked: 4,
            },
            budget_result: "within_targets".into(),
        });
        (case, protocol)
    }

    #[test]
    #[ignore = "heavy; run explicitly"]
    fn top_level_verifier_rejects_rehashed_incorrect_rss_peak() {
        let root = std::env::temp_dir().join(format!(
            "fightbox-sweep-resource-mutation-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let mut report = synthetic_report(&root, SweepMode::Sampled);
        let case = report
            .cases
            .iter_mut()
            .find(|case| case.id == "local-s2-r100")
            .unwrap();
        case.resources.sampled_rss_bytes = vec![1, 7, 2];
        case.resources.peak_sampled_rss_bytes = Some(2);
        write_json_atomic(&root.join("report.json"), &report).unwrap();
        write_json_atomic(
            &root.join("artifacts.json"),
            &build_manifest(&root).unwrap(),
        )
        .unwrap();
        let error = verify_staged(&root).unwrap_err();
        assert!(error.message().contains("maximum raw RSS sample"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn report_content_verifier_rejects_incorrect_rss_peak_without_artifact_io() {
        let mut report = synthetic_report_without_artifacts(SweepMode::Sampled);
        let case = report
            .cases
            .iter_mut()
            .find(|case| case.id == "local-s2-r100")
            .unwrap();
        case.resources.sampled_rss_bytes = vec![1, 7, 2];
        case.resources.peak_sampled_rss_bytes = Some(2);
        let error = verify_canonical_cases(&report).unwrap_err();
        assert!(error.message().contains("maximum raw RSS sample"));
    }

    #[test]
    fn sampled_decision_is_always_provisional_and_final_claim_is_rejected() {
        let mut report = synthetic_report_without_artifacts(SweepMode::Sampled);
        assert_eq!(report.decision.applicability, "provisional_not_applicable");
        assert!(report.decision.kernel_reach.is_none());
        verify_decision(&report).unwrap();

        report.decision = Decision {
            applicability: "final_full_protocol".into(),
            kernel_reach: Some(true),
            monolithic_economics: Some(true),
            decision: "schedule_willis_tower_qualification".into(),
            named_post_mvp_phase: Some("post_mvp_kilometer_approach_qualification".into()),
            decision_required_by: None,
        };
        assert!(verify_decision(&report).is_err());
    }

    #[test]
    fn canonical_plan_rejects_missing_extra_duplicate_relabel_and_settings_drift() {
        let baseline = synthetic_report_without_artifacts(SweepMode::Sampled);
        verify_canonical_cases(&baseline).unwrap();
        for mutation in [
            "missing",
            "extra",
            "duplicate",
            "relabel",
            "settings",
            "traversal",
            "absolute",
        ] {
            let mut report = baseline.clone();
            match mutation {
                "missing" => {
                    report.cases.pop();
                }
                "extra" => {
                    let mut extra = report.cases[0].clone();
                    extra.id = "extra".into();
                    report.cases.push(extra);
                }
                "duplicate" => report.cases.push(report.cases[0].clone()),
                "relabel" => report.cases[0].family = "retained_runtime".into(),
                "settings" => {
                    report.cases[0].requested["path_range_m"] = 999.into();
                    report.cases[0].configuration_hash =
                        hash_json(&report.cases[0].requested).unwrap();
                }
                "traversal" | "absolute" => {
                    let case = report
                        .cases
                        .iter_mut()
                        .find(|case| case.family == "retained_runtime")
                        .unwrap();
                    case.requested["bake_id"] = if mutation == "traversal" {
                        "../outside".into()
                    } else {
                        "/tmp/outside".into()
                    };
                    case.configuration_hash = hash_json(&case.requested).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                verify_canonical_cases(&report).is_err(),
                "mutation {mutation}"
            );
        }
    }

    #[test]
    fn benchmark_adversarial_fields_are_recomputed() {
        let (baseline, protocol) = benchmark_case();
        verify_case(&baseline, SweepMode::Sampled, &protocol).unwrap();
        for mutation in [
            "outer_delivered",
            "requested",
            "finite",
            "raw_n",
            "percentile",
            "counter",
            "blocks",
            "budget",
        ] {
            let mut case = baseline.clone();
            let benchmark = case.benchmark.as_mut().unwrap();
            match mutation {
                "outer_delivered" => case.runtime_delivered = Some(serde_json::json!({"x":1})),
                "requested" => benchmark.requested_settings = serde_json::json!({"x":1}),
                "finite" => benchmark.finite.path_simulation = false,
                "raw_n" => {
                    benchmark.path_simulation.raw_ns.pop();
                    benchmark.path_simulation.derived =
                        percentiles(&benchmark.path_simulation.raw_ns).unwrap();
                }
                "percentile" => benchmark.direct_simulation.derived.p99_ns += 1,
                "counter" => benchmark.finite.reflection_effect_samples_checked = 3,
                "blocks" => benchmark.retained_rendered_blocks = 4,
                "budget" => benchmark.budget_result = "hard_limit_exceeded".into(),
                _ => unreachable!(),
            }
            assert!(
                verify_case(&case, SweepMode::Sampled, &protocol).is_err(),
                "mutation {mutation}"
            );
        }
    }

    #[test]
    fn quality_failed_runtime_retains_and_verifies_raw_evidence() {
        let (mut case, protocol) = benchmark_case();
        case.status = CaseStatus::QualityFailed;
        case.reason = Some("one or more retained offline stage hard limits exceeded".into());
        let benchmark = case.benchmark.as_mut().unwrap();
        benchmark.direct_simulation.raw_ns = vec![20_000_000; 4];
        benchmark.direct_simulation.derived =
            percentiles(&benchmark.direct_simulation.raw_ns).unwrap();
        benchmark.budget_result = "hard_limit_exceeded".into();
        verify_case(&case, SweepMode::Sampled, &protocol).unwrap();
    }

    #[test]
    fn negative_1000m_cell_never_establishes_reach() {
        let mut case = reach_case();
        case.requested["positive_cell"] = false.into();
        case.requested["path_range_m"] = 1_000.into();
        let decision = formal_decision(SweepMode::Full, &[case], "2026-08-05");
        assert_eq!(decision.kernel_reach, Some(false));
        assert_eq!(
            decision.decision,
            "stop_and_replan_narrow_outdoor_backend_before_phase_c"
        );
    }

    #[test]
    fn component_safe_paths_reject_traversal_absolute_and_symlink_escape() {
        let root = temp_test_root("path-confinement");
        fs::create_dir(root.join("cases")).unwrap();
        assert!(safe_case_dir(&root, "../outside").is_err());
        assert!(safe_case_dir(&root, "/tmp/outside").is_err());
        let outside = temp_test_root("outside");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("cases/escaped")).unwrap();
            assert!(safe_case_dir(&root, "escaped").is_err());
            assert!(build_manifest(&root).is_err());
        }
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn symlinked_artifact_root_is_rejected() {
        let root = temp_test_root("symlink-root");
        let outside = temp_test_root("symlink-root-outside");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("cases")).unwrap();
            assert!(build_manifest(&root).is_err());
            let report_link = root.with_extension("report-link");
            std::os::unix::fs::symlink(&outside, &report_link).unwrap();
            assert!(verify_report(&report_link).is_err());
            fs::remove_file(report_link).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn authority_metadata_and_artifact_hash_mutations_are_rejected() {
        let mode = SweepMode::Sampled;
        let protocol = Protocol::for_mode(mode);
        let engine_hash = "a".repeat(64);
        let baseline = synthetic_authority(
            mode,
            &protocol,
            source_identity_sha256().unwrap(),
            engine_hash.clone(),
        );
        verify_authority_metadata(&baseline, mode, &protocol).unwrap();
        verify_authority_artifact_hashes(
            &baseline,
            PINNED_LIBPHONON_SHA256,
            &engine_hash,
            &engine_hash,
            true,
        )
        .unwrap();

        for mutation in ["engine", "source", "plan", "checksum"] {
            let mut authority = baseline.clone();
            match mutation {
                "engine" => authority.engine_identity = "invented".into(),
                "source" => authority.source_identity_sha256 = "0".repeat(64),
                "plan" => authority.canonical_plan_sha256 = "0".repeat(64),
                "checksum" => authority.steam_audio_dylib_sha256 = "0".repeat(64),
                _ => unreachable!(),
            }
            assert!(
                verify_authority_metadata(&authority, mode, &protocol).is_err(),
                "mutation {mutation}"
            );
        }

        let fake_hash = sha256_hex(b"coordinated fake engine");
        assert!(
            verify_authority_artifact_hashes(
                &baseline,
                &"0".repeat(64),
                &engine_hash,
                &engine_hash,
                true,
            )
            .is_err(),
            "mutation fake_sdk"
        );
        assert!(
            verify_authority_artifact_hashes(
                &baseline,
                PINNED_LIBPHONON_SHA256,
                &fake_hash,
                &engine_hash,
                false,
            )
            .is_err(),
            "mutation fake_engine"
        );
        let mut coordinated = baseline;
        coordinated.engine_executable_sha256 = fake_hash.clone();
        assert!(
            verify_authority_artifact_hashes(
                &coordinated,
                PINNED_LIBPHONON_SHA256,
                &fake_hash,
                &engine_hash,
                false,
            )
            .is_err(),
            "mutation coordinated_engine"
        );
    }

    #[test]
    fn report_schema_rejects_unknown_fields() {
        let report = synthetic_report_without_artifacts(SweepMode::Sampled);
        let mut value = serde_json::to_value(report).unwrap();
        value["invented_claim"] = true.into();
        let error = serde_json::from_value::<Report>(value).unwrap_err();
        assert!(error.to_string().contains("unknown field `invented_claim`"));
    }

    #[test]
    fn validation_timeout_without_envelope_is_an_explicit_status() {
        let run = RunOutcome {
            resources: Resources {
                wall_time_ms: Some(CHILD_TIMEOUT_MS),
                child_exit_code: None,
                child_signal: Some(15),
                time_l_max_rss_bytes: Some(1),
                sampled_rss_bytes: vec![1],
                peak_sampled_rss_bytes: Some(1),
                temp_directory_peak_bytes: Some(1),
                termination: Some("timeout".into()),
            },
            reason: Some("isolated child exceeded 15 minute timeout".into()),
        };
        assert_eq!(classify(&run, None), CaseStatus::Timeout);
        verify_exit_status(
            CaseStatus::Timeout,
            &run.resources,
            run.reason.as_deref(),
            "validation",
        )
        .unwrap();
    }

    #[test]
    fn final_disk_sample_over_cap_is_resource_killed() {
        let mut forced = None;
        apply_final_disk_cap(&mut forced, TEMP_CAP + 1);
        assert_eq!(forced.unwrap().0, "resource_killed");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_kills_term_ignoring_descendant_after_leader_exit() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("(trap '' TERM HUP; while :; do sleep 1; done) >/dev/null 2>&1 & exit 0");
        command.process_group(0);
        let mut leader = command.spawn().unwrap();
        let group = leader.id();
        leader.wait().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(!group_members(group).is_empty());
        terminate_group_with_grace(group, Duration::from_millis(50));
        assert!(group_members(group).is_empty());
    }
}
