//! `fightbox` — Phase A CLI entry point.
//!
//! Commands:
//!   `status` / `smoke` — machine-readable backend and gate status JSON. These
//!     never execute S0 or S3.
//!   `help`             — usage.
//!   `phase-a s0`       — render the S0 free-field approach through Steam Audio.
//!   `phase-a s3-bake`  — bake probes for the S3 corner fixture.
//!   `phase-a s3-render` — reload a baked world and render the S3 stems.
//!   `phase-a verify`   — artifact-driven verification of a capture bundle.
//!   `phase-a sweep`    — retained offline stage and kilometer bake sweep.
//!
//! Every artifact-backed command returns a nonzero exit code with a specific
//! error message on any failure.

use std::path::PathBuf;
use std::process::ExitCode;

use fightbox_steam_audio::{BackendAvailability, ReflectionEffectConfig};
use fightbox_steam_audio::{CapabilityStatus, runtime_status, steam_audio_provenance};
use fightbox_world::{CellGridIndex, GeodeticOrigin};

mod anomaly_field;
mod asset;
mod atomicio;
mod bake_memory;
#[cfg(test)]
mod bake_expectations;
mod bake_reservation;
mod bundle;
mod calibrate;
mod canonical_audio;
mod city;
mod city_build;
mod city_place;
mod city_bake_v2;
mod city_oracle;
mod city_route;
mod echo_authority_cli;
mod error;
mod fixture;
mod listening;
mod metrics;
mod neutral_swap_traverse;
mod phase_b;
mod probe_byte_estimate_v2;
mod provenance;
mod s0;
mod s3_bake;
mod s3_render;
mod scene;
mod schema;
mod sweep;
mod verify;

const HELP: &str = "fightbox 0.1.0\n\n\
USAGE:\n    fightbox <COMMAND> [OPTIONS]\n\n\
COMMANDS:\n    status            Print machine-readable backend and gate status JSON\n    \
smoke             Alias for status; it does not execute S0 or S3\n    \
help              Print this help\n    \
asset pack        Verify a source contract and write indexed canonical chunks\n    \
asset inspect     Verify and summarize a canonical chunk package\n    \
phase-a s0        Render the S0 free-field approach through Steam Audio\n    \
phase-a s3-bake   Bake probes for the S3 corner fixture\n    \
phase-a s3-render Reload a baked world and render the S3 stems\n    \
phase-a verify    Verify an S0 or S3 capture bundle from its artifacts\n    \
phase-a sweep     Run the fast sampled retained-stage/km sweep (provisional; no Phase A branch)\n    \
phase-a sweep --mode full\n\
                  Run the expensive exact 12-bake/92-runtime/6+ km protocol\n    \
phase-a sweep --verify <report-directory>\n\
                  Verify a self-contained sweep report without SDK work\n    \
phase-b s6a      Render the deterministic four-source S6a fixture\n    \
                  [--reflection-effect <parametric|convolution>] (default: parametric)\n    \
phase-b s6b      Render the deterministic eight-source S6b fixture\n    \
                  [--reflection-effect <parametric|convolution>] (default: parametric)\n    \
phase-b soak     Run the four-source offline or feature-gated live soak\n    \
                  [--reflection-effect <parametric|convolution>] (default: convolution)\n\n\
phase-b s6b-soak Run the eight-source offline or feature-gated live soak
    \
                  [--reflection-effect <parametric|convolution>] (default: convolution)
    \
phase-b neutral-swap-traverse Run the linked four-cell callback route
    \
                  --route-manifest <path> --artifact-root <path>
    \
                  --oracle-package <path> --oracle-bake <path> --output <directory>

\
listening init --output <directory>\n    \
                  Write blank JSON and Markdown qualification forms\n    \
listening validate <directory>\n    \
                  Validate completed observations and listener sign-off\n\n\
city build --place <text> | --center lat,lon [--radius-m 250] [--heights osm|lidar|auto] --output <directory>\n    \
                  Or --osm <overpass.json> | --bbox south,west,north,east\n    \
                  Heights default to auto: LiDAR where covered, OSM roof parts/height/levels,\n    \
                  then the default. --heights osm opts out of LiDAR; --osm stays offline\n    \
                  [--bake-threads <n>] [--force-rebake] [--allow-large-bake]\n    \
                  [--corridor <route.gpx|route.geojson> [--corridor-width-m 20]] keeps probes\n    \
                  within W m of the walked route plus Point/waypoint islands; probes inside\n    \
                  buildings are dropped unless --keep-interior-probes\n    \
                  --graded [--fine-radius-m 60] [--coarse-spacing-m 8] keeps the full lattice\n    \
                  near the centre, street corners and --corridor routes and its nested sparse\n    \
                  subset elsewhere, repaired so no street or connection the full lattice had\n    \
                  is lost\n    \
                  Geographic builds include LiDAR alley fences, assessor materials where\n    \
                  covered, and rail by default. --materials default opts out of assessor data;\n    \
                  --fences, --materials assessor and --rail also enable detail on local --osm\n    \
                  [--max-bake-memory-gb <GiB>] raises the bake memory cap (default half of RAM);\n    \
                  path bake memory grows with probe pairs, and a bake over the cap is refused\n    \
                  or ended at the cap. Writes probes.json (baked probe positions) beside the launcher\n    \
                  Accepts city bake options; street defaults: path 1500m, visibility 40m,\n    \
                  spacing 4m, height 1.5m, ceiling 3m, no elevated layers, 1 thread (place/center: 4)\n    \
                  Caches OSM responses; writes timings.json and run-<place>.command; sources start OFF\n    \
city compile --geojson <path> --output <path>\n    \
                  Compile GeoJSON into a deterministic v1 .fightbox package\n    \
city compile-v2 --geojson <path> --output <path> --city-id <id>\n    \
                  --origin-latitude-degrees <deg> --origin-longitude-degrees <deg>\n    \
                  --origin-altitude-m <m> [--cell-east-index <i>] [--cell-north-index <i>]\n    \
                  [--probe-policy <graded-policy.json>]\n    \
                  Compile and halo-slice WGS84 GeoJSON into a strict world-package-v2 cell;\n    \
                  optional policy emits the exact phase-locked city-bake-v2 probe plan\n    \
city synth       Generate a deterministic Manhattan-style GeoJSON city\n    \
city inspect     Print a package manifest summary and assumptions\n    \
city export-obj  Export a package mesh as deterministic triangulated OBJ\n    \
city bake        Bake probes for a city package (requires linked-sdk)\n    \
                  [--path-range-m <m>] [--visibility-range-m <m>]\n    \
                  [--visibility-samples <n>] [--visibility-threshold <0..1>]\n    \
                  [--probe-spacing-m <m>] [--probe-height-above-floor-m <m>]\n    \
                  [--probe-ceiling-m <m>] [--bake-threads <n>]\n    \
                  [--elevated-probe-layer-m <m>] (repeatable; adds a flat\n    \
                  mid-air probe layer at that ENU altitude so airborne\n    \
                  sources have influencing probes)\n    \
                  [--elevated-probe-spacing-m <m>] (horizontal spacing of every\n    \
                  elevated layer; requires at least one layer and defaults to\n    \
                  the floor probe spacing)\n    \
                  defaults: 100m, 6m, 1, 0.5, 4m, 1.5m, 3m, no layers, 1 thread\n    \
city bake-v2     Bake the exact indexed graded probe plan (requires linked-sdk)\n    \
                  --package <v2 package> --output <artifact directory>\n    \
                  [--visibility-range-m <m>] [--visibility-samples <n>]\n    \
                  [--visibility-threshold <0..1>] [--probe-visibility-radius-m <m>]\n    \
                  [--bake-threads <n>]\n    \
                  [--probe-byte-model <wave17-fixed-tier-mesh-open-pairs-v2>];\n    \
                  path range is fixed at the 600 m mobile horizon\n    \
city probe-byte-estimate-verify --artifact <baked-v2> --package <v2 package>\n    \
                  Verify the additive estimate against exact package, plan, payload, and installed bytes\n    \
city oracle-bake --geojson <path> --probe-policy <path> --route-manifest <path>\n    \
                  --cell-package <path> (four) --cell-bake <path> (four) --output <directory>\n    \
                  [--expected-probe-count <n>] [--policy-independent-calibration];\n    \
                  custom-policy calibration is marked non-production and requires exact count\n    \
city oracle-verify --artifact <directory>\n    \
                  Revalidate all oracle route/plan/package/batch cross-bindings without SDK work\n    \
city route-assemble --route-id <id> --cell-package <path> (repeat) --output <directory>\n    \
                  [--cell-bake <path> (repeat)] [--owner-home-cell <id>]\n    \
                  [--four-cell-fixture]; bind route/package/bake identities without baking\n    \
city echo-authority --package <world> --output <artifact-directory>\n    \
                  --anchor <id:east,north,up> --listener <id:east,north,up> (repeat)\n    \
                  [--package-output <extended-world-package>]\n    \
                  --source-hole-count <n>; coordinates are package-cell local ENU\n    \
                  Extract deterministic static mesh echo authority offline\n    \
city render      Render a fixture through a packaged and baked city\n\n\
city metamorphic Jitter assumed heights, bake, and assert the occlusion percept\n\n\
anomaly-field sweep\n    \
                  Cheap direct-ray/baked-path proxy; --package --baked --fixture\n    \
                  --source [--source-height-m] [--listener-height-m] [--spacing-m]\n    \
                  [--inspect-position east,north,up] --output <absolute-directory>\n\n\
anomaly-field corner-scan\n    \
                  Two-tier megablock thoroughfare-corner scan; --package --baked\n    \
                  --fixture --source [--source-height-m] [--listener-height-m]\n    \
                  [--fine-corner-count] --output <absolute-directory>\n\n\
SWEEP OUTPUT:\n    <report-directory>/report.json\n    \
<report-directory>/artifacts.json\n    \
<report-directory>/cases/<case-id>/child.json\n";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Report is the bridge from the error::Result world to process exit codes.
    error::report(dispatch(&args[1..]).map(|()| ExitCode::SUCCESS))
}

/// Parse argv (without the program name) and run the requested command.
fn dispatch(args: &[String]) -> error::Result<()> {
    match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{HELP}");
            Ok(())
        }
        Some("status") | Some("smoke") => {
            println!("{}", status_json());
            Ok(())
        }
        Some("phase-a") => dispatch_phase_a(&args[1..]),
        Some("phase-b") => dispatch_phase_b(&args[1..]),
        Some("asset") => canonical_audio::run(&args[1..]),
        Some("listening") => listening::run(&args[1..]),
        Some("city") => dispatch_city(&args[1..]),
        Some("anomaly-field") => anomaly_field::run(&args[1..]),
        Some(command) => Err(error::CliError::new(format!(
            "unknown command: {command}\n\n{HELP}"
        ))),
    }
}

fn dispatch_city(args: &[String]) -> error::Result<()> {
    match args.first().map(String::as_str) {
        Some("build") => city_build::run(&args[1..]),
        Some("synth") => {
            let (seed, blocks, output) = parse_city_synth_args(&args[1..])?;
            city::synth(seed, blocks, &output)
        }
        Some("compile") => {
            let values = parse_named_paths(&args[1..], &["--geojson", "--output"])?;
            city::compile_geojson(&values[0], &values[1])
        }
        Some("compile-v2") => {
            let (geojson, output, config) = parse_city_compile_v2_args(&args[1..])?;
            city::compile_geojson_v2(&geojson, &output, config)
        }
        Some("inspect") => {
            if args.len() != 2 {
                return Err(error::CliError::new("usage: fightbox city inspect <pkg>"));
            }
            city::inspect(PathBuf::from(&args[1]).as_path())
        }
        Some("export-obj") => {
            let values = parse_named_paths(&args[1..], &["--package", "--output"])?;
            city::export_package_obj(&values[0], &values[1])
        }
        Some("bake") => {
            let (package, output, config) = parse_city_bake_args(&args[1..])?;
            city::bake_with_config(&package, &output, config)
        }
        Some("bake-v2") => {
            let (package, output, config) = parse_city_bake_v2_args(&args[1..])?;
            city_bake_v2::bake(&package, &output, config)
        }
        Some("probe-byte-estimate-verify") => {
            let values = parse_named_paths(&args[1..], &["--artifact", "--package"])?;
            city_bake_v2::verify_probe_byte_estimate(&values[0], &values[1])
        }
        Some("oracle-bake") => {
            let config = parse_city_oracle_bake_args(&args[1..])?;
            city_oracle::bake(config)
        }
        Some("oracle-verify") => {
            let values = parse_named_paths(&args[1..], &["--artifact"])?;
            city_oracle::verify_artifact(&values[0])
        }
        Some("route-assemble") => {
            let (config, output) = parse_city_route_assemble_args(&args[1..])?;
            city_route::assemble(config, &output)
        }
        Some("echo-authority") => echo_authority_cli::run(&args[1..]),
        Some("render") => {
            let values = parse_named_paths(
                &args[1..],
                &["--package", "--baked", "--fixture", "--output"],
            )?;
            city::render(&values[0], &values[1], &values[2], &values[3])
        }
        Some("metamorphic") => {
            let values = parse_named_paths(&args[1..], &["--geojson", "--output"])?;
            city::metamorphic(&values[0], &values[1])
        }
        Some(subcommand) => Err(error::CliError::new(format!(
            "unknown city subcommand: {subcommand}\n\n{HELP}"
        ))),
        None => Err(error::CliError::new(format!(
            "city requires a subcommand\n\n{HELP}"
        ))),
    }
}

fn parse_city_oracle_bake_args(args: &[String]) -> error::Result<city_oracle::OracleBakeConfig> {
    let mut geojson = None;
    let mut probe_policy = None;
    let mut route_manifest = None;
    let mut cell_packages = Vec::new();
    let mut cell_bakes = Vec::new();
    let mut output = None;
    let mut visibility_samples = None;
    let mut probe_visibility_radius_m = None;
    let mut visibility_threshold = None;
    let mut visibility_range_m = None;
    let mut bake_threads = None;
    let mut expected_probe_count = None;
    let mut policy_independent_calibration = false;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        if flag == "--policy-independent-calibration" {
            if policy_independent_calibration {
                return Err(error::CliError::new(format!("duplicate argument {flag}")));
            }
            policy_independent_calibration = true;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| error::CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--geojson" => set_once(&mut geojson, PathBuf::from(value), flag)?,
            "--probe-policy" => set_once(&mut probe_policy, PathBuf::from(value), flag)?,
            "--route-manifest" => set_once(&mut route_manifest, PathBuf::from(value), flag)?,
            "--cell-package" => cell_packages.push(PathBuf::from(value)),
            "--cell-bake" => cell_bakes.push(PathBuf::from(value)),
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            "--visibility-samples" => set_once(
                &mut visibility_samples,
                parse_positive_i32(value, flag)?,
                flag,
            )?,
            "--probe-visibility-radius-m" => {
                let radius = value.parse::<f32>().map_err(|_| {
                    error::CliError::new(format!("{flag} requires a non-negative number"))
                })?;
                if !radius.is_finite() || radius < 0.0 {
                    return Err(error::CliError::new(format!(
                        "{flag} requires a finite non-negative number"
                    )));
                }
                set_once(&mut probe_visibility_radius_m, radius, flag)?;
            }
            "--visibility-threshold" => {
                let threshold = value.parse::<f32>().map_err(|_| {
                    error::CliError::new(format!("{flag} requires a number between 0 and 1"))
                })?;
                if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                    return Err(error::CliError::new(format!(
                        "{flag} requires a finite number between 0 and 1"
                    )));
                }
                set_once(&mut visibility_threshold, threshold, flag)?;
            }
            "--visibility-range-m" => set_once(
                &mut visibility_range_m,
                parse_positive_f32(value, flag)?,
                flag,
            )?,
            "--bake-threads" => {
                set_once(&mut bake_threads, parse_positive_i32(value, flag)?, flag)?
            }
            "--expected-probe-count" => {
                let count = value.parse::<u64>().map_err(|_| {
                    error::CliError::new(format!("{flag} requires a positive integer"))
                })?;
                if count == 0 {
                    return Err(error::CliError::new(format!(
                        "{flag} requires a positive integer"
                    )));
                }
                set_once(&mut expected_probe_count, count, flag)?;
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown city oracle-bake argument {other:?}"
                )));
            }
        }
        index += 2;
    }
    let mut config = city_oracle::OracleBakeConfig::with_defaults(
        geojson.ok_or_else(|| error::CliError::new("missing --geojson"))?,
        probe_policy.ok_or_else(|| error::CliError::new("missing --probe-policy"))?,
        route_manifest.ok_or_else(|| error::CliError::new("missing --route-manifest"))?,
        cell_packages,
        cell_bakes,
        output.ok_or_else(|| error::CliError::new("missing --output"))?,
    );
    if let Some(value) = visibility_samples {
        config.visibility_samples = value;
    }
    if let Some(value) = probe_visibility_radius_m {
        config.probe_visibility_radius_m = value;
    }
    if let Some(value) = visibility_threshold {
        config.visibility_threshold = value;
    }
    if let Some(value) = visibility_range_m {
        config.visibility_range_m = value;
    }
    if let Some(value) = bake_threads {
        config.bake_threads = value;
    }
    config.expected_probe_count = expected_probe_count;
    config.policy_independent_calibration = policy_independent_calibration;
    Ok(config)
}

fn parse_city_route_assemble_args(
    args: &[String],
) -> error::Result<(city_route::RouteAssemblyConfig, PathBuf)> {
    let mut route_id = None;
    let mut cell_packages = Vec::new();
    let mut cell_bakes = Vec::new();
    let mut output = None;
    let mut owner_home_cell_id = None;
    let mut include_four_cell_fixture = false;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        if flag == "--four-cell-fixture" {
            if include_four_cell_fixture {
                return Err(error::CliError::new(format!("duplicate argument {flag}")));
            }
            include_four_cell_fixture = true;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| error::CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--route-id" => set_once(&mut route_id, value.clone(), flag)?,
            "--cell-package" => cell_packages.push(PathBuf::from(value)),
            "--cell-bake" => cell_bakes.push(PathBuf::from(value)),
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            "--owner-home-cell" => {
                set_once(&mut owner_home_cell_id, value.clone(), flag)?;
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown city route-assemble argument {other:?}"
                )));
            }
        }
        index += 2;
    }
    Ok((
        city_route::RouteAssemblyConfig {
            route_id: route_id
                .filter(|value: &String| !value.is_empty())
                .ok_or_else(|| error::CliError::new("missing required --route-id <id>"))?,
            cell_packages,
            cell_bakes,
            owner_home_cell_id,
            include_four_cell_fixture,
        },
        output.ok_or_else(|| error::CliError::new("missing required --output <directory>"))?,
    ))
}

fn parse_city_bake_v2_args(
    args: &[String],
) -> error::Result<(PathBuf, PathBuf, city_bake_v2::BakeV2Config)> {
    let mut package = None;
    let mut output = None;
    let mut visibility_samples = None;
    let mut probe_visibility_radius_m = None;
    let mut visibility_threshold = None;
    let mut visibility_range_m = None;
    let mut bake_threads = None;
    let mut probe_byte_model = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| error::CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--package" => set_once(&mut package, PathBuf::from(value), flag)?,
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            "--visibility-samples" => set_once(
                &mut visibility_samples,
                parse_positive_i32(value, flag)?,
                flag,
            )?,
            "--probe-visibility-radius-m" => {
                let radius = value.parse::<f32>().map_err(|_| {
                    error::CliError::new(format!("{flag} requires a non-negative number"))
                })?;
                if !radius.is_finite() || radius < 0.0 {
                    return Err(error::CliError::new(format!(
                        "{flag} requires a finite non-negative number"
                    )));
                }
                set_once(&mut probe_visibility_radius_m, radius, flag)?;
            }
            "--visibility-threshold" => {
                let threshold = value.parse::<f32>().map_err(|_| {
                    error::CliError::new(format!("{flag} requires a number between 0 and 1"))
                })?;
                if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                    return Err(error::CliError::new(format!(
                        "{flag} requires a finite number between 0 and 1"
                    )));
                }
                set_once(&mut visibility_threshold, threshold, flag)?;
            }
            "--visibility-range-m" => set_once(
                &mut visibility_range_m,
                parse_positive_f32(value, flag)?,
                flag,
            )?,
            "--bake-threads" => {
                set_once(&mut bake_threads, parse_positive_i32(value, flag)?, flag)?
            }
            "--probe-byte-model" => {
                let model = match value.as_str() {
                    "wave17-fixed-tier-mesh-open-pairs-v2" => {
                        city_bake_v2::ProbeByteModelSelection::Wave17FixedTierMeshOpenPairsV2
                    }
                    other => {
                        return Err(error::CliError::new(format!(
                            "{flag} must be wave17-fixed-tier-mesh-open-pairs-v2 (got {other:?})"
                        )));
                    }
                };
                set_once(&mut probe_byte_model, model, flag)?;
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown city bake-v2 argument {other:?}"
                )));
            }
        }
    }
    let defaults = city_bake_v2::BakeV2Config::default();
    Ok((
        package.ok_or_else(|| error::CliError::new("missing required --package <path>"))?,
        output.ok_or_else(|| error::CliError::new("missing required --output <path>"))?,
        city_bake_v2::BakeV2Config {
            visibility_samples: visibility_samples.unwrap_or(defaults.visibility_samples),
            probe_visibility_radius_m: probe_visibility_radius_m
                .unwrap_or(defaults.probe_visibility_radius_m),
            visibility_threshold: visibility_threshold.unwrap_or(defaults.visibility_threshold),
            visibility_range_m: visibility_range_m.unwrap_or(defaults.visibility_range_m),
            bake_threads: bake_threads.unwrap_or(defaults.bake_threads),
            probe_byte_model: probe_byte_model.unwrap_or_default(),
        },
    ))
}

fn parse_city_bake_args(args: &[String]) -> error::Result<(PathBuf, PathBuf, city::BakeConfig)> {
    let mut package = None;
    let mut output = None;
    let mut path_range_m = None;
    let mut visibility_range_m = None;
    let mut visibility_samples = None;
    let mut visibility_threshold = None;
    let mut probe_spacing_m = None;
    let mut probe_height_above_floor_m = None;
    let mut probe_ceiling_m = None;
    let mut elevated_probe_layers_m: Vec<f32> = Vec::new();
    let mut elevated_probe_spacing_m = None;
    let mut bake_threads = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| error::CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--package" => set_once(&mut package, PathBuf::from(value), flag)?,
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            "--path-range-m" => {
                set_once(&mut path_range_m, parse_positive_f32(value, flag)?, flag)?
            }
            "--visibility-range-m" => set_once(
                &mut visibility_range_m,
                parse_positive_f32(value, flag)?,
                flag,
            )?,
            "--visibility-samples" => set_once(
                &mut visibility_samples,
                parse_positive_i32(value, flag)?,
                flag,
            )?,
            "--visibility-threshold" => {
                let threshold = value.parse::<f32>().map_err(|_| {
                    error::CliError::new(format!("{flag} requires a number between 0 and 1"))
                })?;
                if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                    return Err(error::CliError::new(format!(
                        "{flag} requires a finite number between 0 and 1"
                    )));
                }
                set_once(&mut visibility_threshold, threshold, flag)?;
            }
            "--probe-spacing-m" => {
                set_once(&mut probe_spacing_m, parse_positive_f32(value, flag)?, flag)?
            }
            "--probe-height-above-floor-m" => set_once(
                &mut probe_height_above_floor_m,
                parse_positive_f32(value, flag)?,
                flag,
            )?,
            "--probe-ceiling-m" => {
                set_once(&mut probe_ceiling_m, parse_positive_f32(value, flag)?, flag)?
            }
            // Repeatable by design: one flag per layer, so several altitudes can
            // be requested without inventing a list syntax.
            "--elevated-probe-layer-m" => {
                let height = parse_positive_f32(value, flag)?;
                if elevated_probe_layers_m
                    .iter()
                    .any(|existing| *existing == height)
                {
                    return Err(error::CliError::new(format!(
                        "duplicate {flag} altitude {height}"
                    )));
                }
                elevated_probe_layers_m.push(height);
            }
            // One spacing for every layer, unlike the repeatable altitude flag:
            // a per-layer spacing would need a pairing syntax to earn its keep.
            "--elevated-probe-spacing-m" => set_once(
                &mut elevated_probe_spacing_m,
                parse_positive_f32(value, flag)?,
                flag,
            )?,
            "--bake-threads" => {
                set_once(&mut bake_threads, parse_positive_i32(value, flag)?, flag)?
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown city bake argument {other:?}"
                )));
            }
        }
    }
    if elevated_probe_spacing_m.is_some() && elevated_probe_layers_m.is_empty() {
        return Err(error::CliError::new(
            "--elevated-probe-spacing-m requires at least one --elevated-probe-layer-m",
        ));
    }
    let defaults = city::BakeConfig::default();
    Ok((
        package.ok_or_else(|| error::CliError::new("missing required --package <path>"))?,
        output.ok_or_else(|| error::CliError::new("missing required --output <path>"))?,
        city::BakeConfig {
            path_range_m: path_range_m.unwrap_or(defaults.path_range_m),
            visibility_range_m: visibility_range_m.unwrap_or(defaults.visibility_range_m),
            visibility_samples: visibility_samples.unwrap_or(defaults.visibility_samples),
            visibility_threshold: visibility_threshold.unwrap_or(defaults.visibility_threshold),
            probe_spacing_m: probe_spacing_m.unwrap_or(defaults.probe_spacing_m),
            probe_height_above_floor_m: probe_height_above_floor_m
                .unwrap_or(defaults.probe_height_above_floor_m),
            probe_ceiling_m: probe_ceiling_m.unwrap_or(defaults.probe_ceiling_m),
            elevated_probe_layers_m,
            elevated_probe_spacing_m,
            bake_threads: bake_threads.unwrap_or(defaults.bake_threads),
        },
    ))
}

fn parse_city_compile_v2_args(
    args: &[String],
) -> error::Result<(PathBuf, PathBuf, city::CityCompileV2Config)> {
    let mut geojson = None;
    let mut output = None;
    let mut city_id = None;
    let mut origin_latitude_degrees = None;
    let mut origin_longitude_degrees = None;
    let mut origin_altitude_m = None;
    let mut cell_east_index = None;
    let mut cell_north_index = None;
    let mut probe_policy = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| error::CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--geojson" => set_once(&mut geojson, PathBuf::from(value), flag)?,
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            "--city-id" => set_once(&mut city_id, value.clone(), flag)?,
            "--origin-latitude-degrees" => set_once(
                &mut origin_latitude_degrees,
                parse_finite_f64(value, flag)?,
                flag,
            )?,
            "--origin-longitude-degrees" => set_once(
                &mut origin_longitude_degrees,
                parse_finite_f64(value, flag)?,
                flag,
            )?,
            "--origin-altitude-m" => {
                set_once(&mut origin_altitude_m, parse_finite_f64(value, flag)?, flag)?
            }
            "--cell-east-index" => set_once(&mut cell_east_index, parse_i32(value, flag)?, flag)?,
            "--cell-north-index" => set_once(&mut cell_north_index, parse_i32(value, flag)?, flag)?,
            "--probe-policy" => set_once(&mut probe_policy, PathBuf::from(value), flag)?,
            other => {
                return Err(error::CliError::new(format!(
                    "unknown city compile-v2 argument {other:?}"
                )));
            }
        }
    }
    let geodetic_origin = GeodeticOrigin::wgs84(
        origin_latitude_degrees.ok_or_else(|| {
            error::CliError::new("missing required --origin-latitude-degrees <deg>")
        })?,
        origin_longitude_degrees.ok_or_else(|| {
            error::CliError::new("missing required --origin-longitude-degrees <deg>")
        })?,
        origin_altitude_m
            .ok_or_else(|| error::CliError::new("missing required --origin-altitude-m <m>"))?,
    );
    geodetic_origin
        .validate()
        .map_err(|error| error::CliError::new(format!("invalid geodetic origin: {error}")))?;
    Ok((
        geojson.ok_or_else(|| error::CliError::new("missing required --geojson <path>"))?,
        output.ok_or_else(|| error::CliError::new("missing required --output <path>"))?,
        city::CityCompileV2Config {
            city_id: city_id
                .filter(|value| !value.is_empty())
                .ok_or_else(|| error::CliError::new("missing required --city-id <id>"))?,
            geodetic_origin,
            cell_grid_index: CellGridIndex {
                east: cell_east_index.unwrap_or(0),
                north: cell_north_index.unwrap_or(0),
            },
            probe_policy,
        },
    ))
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> error::Result<()> {
    if slot.replace(value).is_some() {
        Err(error::CliError::new(format!("duplicate argument {flag}")))
    } else {
        Ok(())
    }
}

fn parse_positive_f32(value: &str, flag: &str) -> error::Result<f32> {
    let value = value
        .parse::<f32>()
        .map_err(|_| error::CliError::new(format!("{flag} requires a positive number")))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(error::CliError::new(format!(
            "{flag} requires a finite positive number"
        )));
    }
    Ok(value)
}

fn parse_positive_i32(value: &str, flag: &str) -> error::Result<i32> {
    let value = value
        .parse::<i32>()
        .map_err(|_| error::CliError::new(format!("{flag} requires a positive integer")))?;
    if value <= 0 {
        return Err(error::CliError::new(format!(
            "{flag} requires a positive integer"
        )));
    }
    Ok(value)
}

fn parse_finite_f64(value: &str, flag: &str) -> error::Result<f64> {
    let value = value
        .parse::<f64>()
        .map_err(|_| error::CliError::new(format!("{flag} requires a finite number")))?;
    if !value.is_finite() {
        return Err(error::CliError::new(format!(
            "{flag} requires a finite number"
        )));
    }
    Ok(value)
}

fn parse_i32(value: &str, flag: &str) -> error::Result<i32> {
    value
        .parse::<i32>()
        .map_err(|_| error::CliError::new(format!("{flag} requires a signed 32-bit integer")))
}

fn parse_city_synth_args(args: &[String]) -> error::Result<(u64, (u32, u32), PathBuf)> {
    let mut seed = None;
    let mut blocks = None;
    let mut output = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| error::CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--seed" => {
                seed = Some(value.parse::<u64>().map_err(|_| {
                    error::CliError::new("--seed requires an unsigned 64-bit integer")
                })?);
            }
            "--blocks" => {
                let (width, height) = value.split_once('x').ok_or_else(|| {
                    error::CliError::new("--blocks requires dimensions formatted WxH")
                })?;
                let width = width.parse::<u32>().map_err(|_| {
                    error::CliError::new("--blocks requires positive integer dimensions")
                })?;
                let height = height.parse::<u32>().map_err(|_| {
                    error::CliError::new("--blocks requires positive integer dimensions")
                })?;
                if width == 0 || height == 0 {
                    return Err(error::CliError::new(
                        "--blocks dimensions must both be positive",
                    ));
                }
                blocks = Some((width, height));
            }
            "--output" => output = Some(PathBuf::from(value)),
            other => {
                return Err(error::CliError::new(format!(
                    "unknown city synth argument {other:?}; expected --seed, --blocks, --output"
                )));
            }
        }
    }
    Ok((
        seed.ok_or_else(|| error::CliError::new("missing required --seed <N>"))?,
        blocks.ok_or_else(|| error::CliError::new("missing required --blocks <WxH>"))?,
        output.ok_or_else(|| error::CliError::new("missing required --output <path>"))?,
    ))
}

fn parse_named_paths(args: &[String], flags: &[&str]) -> error::Result<Vec<PathBuf>> {
    let mut values = vec![None; flags.len()];
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let index = flags
            .iter()
            .position(|expected| flag == expected)
            .ok_or_else(|| {
                error::CliError::new(format!(
                    "unknown argument {flag:?}; expected {}",
                    flags.join(", ")
                ))
            })?;
        if values[index].is_some() {
            return Err(error::CliError::new(format!(
                "duplicate argument {}",
                flags[index]
            )));
        }
        values[index] = Some(PathBuf::from(iter.next().ok_or_else(|| {
            error::CliError::new(format!("{} requires a path", flags[index]))
        })?));
    }
    values
        .into_iter()
        .zip(flags)
        .map(|(value, flag)| {
            value.ok_or_else(|| error::CliError::new(format!("missing required {flag} <path>")))
        })
        .collect()
}

/// Dispatch the `phase-b` B2 evidence family.
fn dispatch_phase_b(args: &[String]) -> error::Result<()> {
    match args.first().map(String::as_str) {
        Some("s6a") => {
            let (fixture, output, isolation_check, reflection_effect) =
                parse_phase_b_s6a_args(&args[1..])?;
            phase_b::run_s6a(&fixture, &output, isolation_check, reflection_effect)
        }
        Some("s6b") => {
            let (fixture, output, isolation_check, reflection_effect) =
                parse_phase_b_s6a_args(&args[1..])?;
            phase_b::run_s6b(&fixture, &output, isolation_check, reflection_effect)
        }
        Some("soak") => {
            let (minutes, output, live, reflection_effect) = parse_phase_b_soak_args(&args[1..])?;
            phase_b::run_soak(minutes, &output, live, reflection_effect)
        }
        Some("s6b-soak") => {
            let (minutes, output, live, reflection_effect) = parse_phase_b_soak_args(&args[1..])?;
            phase_b::run_s6b_soak(minutes, &output, live, reflection_effect)
        }
        Some("neutral-swap-traverse") => {
            let config = parse_neutral_swap_traverse_args(&args[1..])?;
            neutral_swap_traverse::run(config)
        }
        Some(sub) => Err(error::CliError::new(format!(
            "unknown phase-b subcommand: {sub}\n\n{HELP}"
        ))),
        None => Err(error::CliError::new(format!(
            "phase-b requires a subcommand\n\n{HELP}"
        ))),
    }
}

#[derive(Debug)]
struct NeutralSwapTraverseArgs {
    route_manifest: PathBuf,
    artifact_root: PathBuf,
    oracle_package: PathBuf,
    oracle_bake: PathBuf,
    output: PathBuf,
}

fn parse_neutral_swap_traverse_args(args: &[String]) -> error::Result<NeutralSwapTraverseArgs> {
    let mut route_manifest = None;
    let mut artifact_root = None;
    let mut oracle_package = None;
    let mut oracle_bake = None;
    let mut output = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| error::CliError::new(format!("{flag} requires a path")))?;
        let target = match flag.as_str() {
            "--route-manifest" => &mut route_manifest,
            "--artifact-root" => &mut artifact_root,
            "--oracle-package" => &mut oracle_package,
            "--oracle-bake" => &mut oracle_bake,
            "--output" => &mut output,
            other => {
                return Err(error::CliError::new(format!(
                    "unknown argument {other:?}; expected --route-manifest, --artifact-root, --oracle-package, --oracle-bake, and --output"
                )));
            }
        };
        if target.is_some() {
            return Err(error::CliError::new(format!("duplicate argument {flag}")));
        }
        *target = Some(PathBuf::from(value));
    }
    Ok(NeutralSwapTraverseArgs {
        route_manifest: route_manifest
            .ok_or_else(|| error::CliError::new("missing required --route-manifest <path>"))?,
        artifact_root: artifact_root
            .ok_or_else(|| error::CliError::new("missing required --artifact-root <path>"))?,
        oracle_package: oracle_package
            .ok_or_else(|| error::CliError::new("missing required --oracle-package <path>"))?,
        oracle_bake: oracle_bake
            .ok_or_else(|| error::CliError::new("missing required --oracle-bake <path>"))?,
        output: output
            .ok_or_else(|| error::CliError::new("missing required --output <directory>"))?,
    })
}

fn parse_reflection_effect(value: &str) -> error::Result<ReflectionEffectConfig> {
    match value {
        "parametric" => Ok(ReflectionEffectConfig::PARAMETRIC),
        "convolution" => Ok(ReflectionEffectConfig::CONVOLUTION),
        other => Err(error::CliError::new(format!(
            "invalid --reflection-effect {other:?}; expected parametric or convolution"
        ))),
    }
}

fn parse_phase_b_s6a_args(
    args: &[String],
) -> error::Result<(PathBuf, PathBuf, bool, ReflectionEffectConfig)> {
    let mut fixture = None;
    let mut output = None;
    let mut isolation_check = false;
    let mut reflection_effect = ReflectionEffectConfig::PARAMETRIC;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--fixture" => {
                fixture =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        error::CliError::new("--fixture requires a path")
                    })?));
            }
            "--output" => {
                output =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        error::CliError::new("--output requires a path")
                    })?));
            }
            "--isolation-check" => isolation_check = true,
            "--reflection-effect" => {
                reflection_effect = parse_reflection_effect(iter.next().ok_or_else(|| {
                    error::CliError::new("--reflection-effect requires parametric or convolution")
                })?)?;
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown argument {other:?}; expected --fixture, --output, optional --isolation-check, and optional --reflection-effect <parametric|convolution>"
                )));
            }
        }
    }
    Ok((
        fixture.ok_or_else(|| error::CliError::new("missing required --fixture <path>"))?,
        output.ok_or_else(|| error::CliError::new("missing required --output <path>"))?,
        isolation_check,
        reflection_effect,
    ))
}

fn parse_phase_b_soak_args(
    args: &[String],
) -> error::Result<(u64, PathBuf, bool, ReflectionEffectConfig)> {
    let mut minutes = None;
    let mut output = None;
    let mut live = false;
    let mut reflection_effect = ReflectionEffectConfig::CONVOLUTION;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--minutes" => {
                let value = iter
                    .next()
                    .ok_or_else(|| error::CliError::new("--minutes requires a positive integer"))?;
                minutes =
                    Some(value.parse::<u64>().map_err(|_| {
                        error::CliError::new("--minutes requires a positive integer")
                    })?);
            }
            "--output" => {
                output =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        error::CliError::new("--output requires a path")
                    })?));
            }
            "--live" => live = true,
            "--reflection-effect" => {
                reflection_effect = parse_reflection_effect(iter.next().ok_or_else(|| {
                    error::CliError::new("--reflection-effect requires parametric or convolution")
                })?)?;
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown argument {other:?}; expected --minutes, --output, optional --live, and optional --reflection-effect <parametric|convolution>"
                )));
            }
        }
    }
    Ok((
        minutes.ok_or_else(|| error::CliError::new("missing required --minutes <N>"))?,
        output.ok_or_else(|| error::CliError::new("missing required --output <path>"))?,
        live,
        reflection_effect,
    ))
}

/// Dispatch the `phase-a` subcommand family.
fn dispatch_phase_a(args: &[String]) -> error::Result<()> {
    match args.first().map(String::as_str) {
        Some("s0") => {
            let (fixture, out) = parse_s0_args(&args[1..])?;
            s0::run(&fixture, &out)
        }
        Some("s3-bake") => {
            let (fixture, out) = parse_s0_args(&args[1..])?;
            s3_bake::run(&fixture, &out)
        }
        Some("s3-render") => {
            let (fixture, world, out) = parse_s3_render_args(&args[1..])?;
            s3_render::run(&fixture, &world, &out)
        }
        Some("verify") => {
            let (bundle, mechanical_only) = parse_verify_args(&args[1..])?;
            let result = verify::run(&bundle, mechanical_only)?;
            println!("{result}");
            Ok(())
        }
        Some("sweep")
            if args
                .get(1)
                .is_some_and(|arg| matches!(arg.as_str(), "help" | "--help" | "-h")) =>
        {
            print!("{HELP}");
            Ok(())
        }
        Some("sweep") => sweep::run(sweep::parse_args(&args[1..])?),
        Some("__sweep-child") => sweep::run_child(&args[1..]),
        Some(sub) => Err(error::CliError::new(format!(
            "unknown phase-a subcommand: {sub}\n\n{HELP}"
        ))),
        None => Err(error::CliError::new(format!(
            "phase-a requires a subcommand\n\n{HELP}"
        ))),
    }
}

/// Parse `--fixture <path> --out <path>` for s0 and s3-bake.
fn parse_s0_args(args: &[String]) -> error::Result<(PathBuf, PathBuf)> {
    let mut fixture: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--fixture" => {
                let value = iter
                    .next()
                    .ok_or_else(|| error::CliError::new("--fixture requires a path"))?;
                fixture = Some(PathBuf::from(value));
            }
            "--out" => {
                let value = iter
                    .next()
                    .ok_or_else(|| error::CliError::new("--out requires a path"))?;
                out = Some(PathBuf::from(value));
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown argument {other:?}; expected --fixture and --out"
                )));
            }
        }
    }
    let fixture =
        fixture.ok_or_else(|| error::CliError::new("missing required --fixture <path>"))?;
    let out = out.ok_or_else(|| error::CliError::new("missing required --out <path>"))?;
    Ok((fixture, out))
}

/// Parse `--fixture <path> --world <path> --out <path>` for s3-render.
fn parse_s3_render_args(args: &[String]) -> error::Result<(PathBuf, PathBuf, PathBuf)> {
    let mut fixture: Option<PathBuf> = None;
    let mut world: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--fixture" => {
                let value = iter
                    .next()
                    .ok_or_else(|| error::CliError::new("--fixture requires a path"))?;
                fixture = Some(PathBuf::from(value));
            }
            "--world" => {
                let value = iter
                    .next()
                    .ok_or_else(|| error::CliError::new("--world requires a path"))?;
                world = Some(PathBuf::from(value));
            }
            "--out" => {
                let value = iter
                    .next()
                    .ok_or_else(|| error::CliError::new("--out requires a path"))?;
                out = Some(PathBuf::from(value));
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown argument {other:?}; expected --fixture, --world, and --out"
                )));
            }
        }
    }
    let fixture =
        fixture.ok_or_else(|| error::CliError::new("missing required --fixture <path>"))?;
    let world = world.ok_or_else(|| error::CliError::new("missing required --world <path>"))?;
    let out = out.ok_or_else(|| error::CliError::new("missing required --out <path>"))?;
    Ok((fixture, world, out))
}

/// Parse `--bundle <path> [--mechanical-only]` for verify.
fn parse_verify_args(args: &[String]) -> error::Result<(PathBuf, bool)> {
    let mut bundle: Option<PathBuf> = None;
    let mut mechanical_only = false;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--bundle" => {
                let value = iter
                    .next()
                    .ok_or_else(|| error::CliError::new("--bundle requires a path"))?;
                bundle = Some(PathBuf::from(value));
            }
            "--mechanical-only" => {
                mechanical_only = true;
            }
            other => {
                return Err(error::CliError::new(format!(
                    "unknown argument {other:?}; expected --bundle and optional --mechanical-only"
                )));
            }
        }
    }
    let bundle = bundle.ok_or_else(|| error::CliError::new("missing required --bundle <path>"))?;
    Ok((bundle, mechanical_only))
}

fn status_json() -> String {
    let status = runtime_status();
    let (version, commit) = steam_audio_provenance();
    let backend = match status.backend {
        BackendAvailability::Available {
            version,
            upstream_commit,
        } => format!(
            r#"{{"status":"available","version":"{version}","upstream_commit":"{upstream_commit}"}}"#
        ),
        BackendAvailability::Unavailable(metadata) => format!(
            r#"{{"status":"unavailable","reason":"{}","expected_version":"{}","upstream_commit":"{}"}}"#,
            metadata.reason, metadata.expected_version, metadata.upstream_commit
        ),
    };
    format!(
        r#"{{"schema_version":"fightbox.cli-status.v1","backend":{backend},"version_provenance":{{"steam_audio_version":"{version}","upstream_commit":"{commit}"}},"capabilities":{{"direct":"{}","reflections":"{}","baked_pathing":"{}"}},"gates":{{"S0":"{}","S3":"{}"}},"claims":[],"non_claims":["This command does not execute S0.","This command does not execute S3 or a path bake."]}}"#,
        capability_name(status.direct),
        capability_name(status.reflections),
        capability_name(status.baked_pathing),
        status.s0.as_str(),
        status.s3.as_str()
    )
}

fn capability_name(value: CapabilityStatus) -> &'static str {
    match value {
        CapabilityStatus::Available => "available",
        CapabilityStatus::Unavailable { .. } => "unavailable",
        CapabilityStatus::NotEstablished { .. } => "not_established",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_machine_readable_json_with_unrun_gates() {
        let json = status_json();
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(
            json.contains(r#""backend":{\"#) == false,
            "must not double encode backend"
        );
        assert!(json.contains(r#""S0":"not_run""#));
        assert!(json.contains(r#""S3":"not_run""#));
    }

    #[test]
    fn help_names_sweep_modes_and_report_paths() {
        assert!(HELP.contains("phase-a sweep --mode full"));
        assert!(HELP.contains("phase-a sweep --verify <report-directory>"));
        assert!(HELP.contains("<report-directory>/report.json"));
        assert!(HELP.contains("city synth"));
        assert!(HELP.contains("phase-b s6b"));
        assert!(HELP.contains("phase-b s6b-soak"));
        assert!(HELP.contains("listening init --output"));
        assert!(HELP.contains("listening validate <directory>"));
        assert!(HELP.contains("asset pack"));
        assert!(HELP.contains("city compile --geojson <path> --output <path>"));
        assert!(HELP.contains("city compile-v2 --geojson <path>"));
        assert!(HELP.contains("city oracle-bake --geojson <path>"));
        assert!(HELP.contains("city oracle-verify --artifact <directory>"));
        assert!(HELP.contains("city echo-authority --package <world>"));
    }

    #[test]
    fn dispatch_help_with_no_args() {
        // No args should print help and succeed.
        assert!(dispatch(&[]).is_ok());
    }

    #[test]
    fn dispatch_phase_a_sweep_help_succeeds_without_running_a_sweep() {
        assert!(dispatch(&["phase-a".into(), "sweep".into(), "--help".into()]).is_ok());
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        assert!(dispatch(&["bogus".to_string()]).is_err());
    }

    #[test]
    fn parses_city_synth_arguments() {
        let (seed, blocks, output) = parse_city_synth_args(&[
            "--seed".into(),
            "7".into(),
            "--blocks".into(),
            "6x4".into(),
            "--output".into(),
            "city.geojson".into(),
        ])
        .unwrap();
        assert_eq!(seed, 7);
        assert_eq!(blocks, (6, 4));
        assert_eq!(output, PathBuf::from("city.geojson"));
        assert!(parse_city_synth_args(&["--blocks".into(), "6".into()]).is_err());
        assert!(parse_city_synth_args(&["--blocks".into(), "0x6".into()]).is_err());
    }

    #[test]
    fn parses_city_compile_v2_identity_origin_and_grid() {
        let (geojson, output, config) = parse_city_compile_v2_args(&[
            "--geojson".into(),
            "cell.geojson".into(),
            "--output".into(),
            "cell.fightbox".into(),
            "--city-id".into(),
            "chicago-loop".into(),
            "--origin-latitude-degrees".into(),
            "41.881832".into(),
            "--origin-longitude-degrees".into(),
            "-87.623177".into(),
            "--origin-altitude-m".into(),
            "181".into(),
            "--cell-east-index".into(),
            "2".into(),
            "--cell-north-index".into(),
            "-1".into(),
            "--probe-policy".into(),
            "graded-policy.json".into(),
        ])
        .unwrap();
        assert_eq!(geojson, PathBuf::from("cell.geojson"));
        assert_eq!(output, PathBuf::from("cell.fightbox"));
        assert_eq!(config.city_id, "chicago-loop");
        assert_eq!(
            config.geodetic_origin,
            GeodeticOrigin::wgs84(41.881_832, -87.623_177, 181.0)
        );
        assert_eq!(config.cell_grid_index, CellGridIndex { east: 2, north: -1 });
        assert_eq!(
            config.probe_policy,
            Some(PathBuf::from("graded-policy.json"))
        );
        assert!(
            parse_city_compile_v2_args(&[
                "--geojson".into(),
                "cell.geojson".into(),
                "--output".into(),
                "cell.fightbox".into(),
                "--city-id".into(),
                "chicago-loop".into(),
                "--origin-latitude-degrees".into(),
                "91".into(),
                "--origin-longitude-degrees".into(),
                "-87".into(),
                "--origin-altitude-m".into(),
                "0".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_monolithic_oracle_without_mobile_path_override() {
        let mut args = vec![
            "--geojson".into(),
            "route.geojson".into(),
            "--probe-policy".into(),
            "policy.json".into(),
            "--route-manifest".into(),
            "route.json".into(),
            "--output".into(),
            "oracle".into(),
            "--bake-threads".into(),
            "4".into(),
        ];
        for index in 0..4 {
            args.extend(["--cell-package".into(), format!("cell-{index}.fightbox")]);
        }
        for index in 0..4 {
            args.extend(["--cell-bake".into(), format!("cell-{index}.baked")]);
        }
        let config = parse_city_oracle_bake_args(&args).unwrap();
        assert_eq!(config.geojson, PathBuf::from("route.geojson"));
        assert_eq!(config.probe_policy, PathBuf::from("policy.json"));
        assert_eq!(config.route_manifest, PathBuf::from("route.json"));
        assert_eq!(config.cell_packages.len(), 4);
        assert_eq!(config.cell_bakes.len(), 4);
        assert_eq!(config.bake_threads, 4);
        assert!(
            parse_city_oracle_bake_args(&["--geojson".into(), "route.geojson".into()]).is_err()
        );
    }

    #[test]
    fn parses_city_bake_v2_probe_byte_model_selection() {
        let (_, _, defaults) = parse_city_bake_v2_args(&[
            "--package".into(),
            "city.fightbox".into(),
            "--output".into(),
            "city.baked".into(),
        ])
        .unwrap();
        assert_eq!(
            defaults.probe_byte_model,
            city_bake_v2::ProbeByteModelSelection::Provisional
        );
        let (_, _, calibrated) = parse_city_bake_v2_args(&[
            "--package".into(),
            "city.fightbox".into(),
            "--output".into(),
            "city.baked".into(),
            "--probe-byte-model".into(),
            "wave17-fixed-tier-mesh-open-pairs-v2".into(),
        ])
        .unwrap();
        assert_eq!(
            calibrated.probe_byte_model,
            city_bake_v2::ProbeByteModelSelection::Wave17FixedTierMeshOpenPairsV2
        );
        assert!(
            parse_city_bake_v2_args(&[
                "--package".into(),
                "city.fightbox".into(),
                "--output".into(),
                "city.baked".into(),
                "--probe-byte-model".into(),
                "unknown".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_city_bake_defaults_and_tuning() {
        let (package, output, defaults) = parse_city_bake_args(&[
            "--package".into(),
            "city.fightbox".into(),
            "--output".into(),
            "city.baked".into(),
        ])
        .unwrap();
        assert_eq!(package, PathBuf::from("city.fightbox"));
        assert_eq!(output, PathBuf::from("city.baked"));
        assert_eq!(defaults, city::BakeConfig::default());

        let (_, _, tuned) = parse_city_bake_args(&[
            "--package".into(),
            "city.fightbox".into(),
            "--output".into(),
            "city.baked".into(),
            "--path-range-m".into(),
            "600".into(),
            "--visibility-range-m".into(),
            "20".into(),
            "--visibility-samples".into(),
            "4".into(),
            "--visibility-threshold".into(),
            "0.25".into(),
            "--probe-spacing-m".into(),
            "8".into(),
            "--probe-height-above-floor-m".into(),
            "3".into(),
            "--probe-ceiling-m".into(),
            "63".into(),
            "--elevated-probe-layer-m".into(),
            "30".into(),
            "--elevated-probe-layer-m".into(),
            "80".into(),
            "--bake-threads".into(),
            "10".into(),
        ])
        .unwrap();
        assert_eq!(
            tuned,
            city::BakeConfig {
                path_range_m: 600.0,
                visibility_range_m: 20.0,
                visibility_samples: 4,
                visibility_threshold: 0.25,
                probe_spacing_m: 8.0,
                probe_height_above_floor_m: 3.0,
                probe_ceiling_m: 63.0,
                elevated_probe_layers_m: vec![30.0, 80.0],
                elevated_probe_spacing_m: None,
                bake_threads: 10,
            }
        );
    }

    #[test]
    fn city_bake_accepts_an_independent_elevated_probe_spacing() {
        let (_, _, config) = parse_city_bake_args(&[
            "--package".into(),
            "city.fightbox".into(),
            "--output".into(),
            "city.baked".into(),
            "--probe-spacing-m".into(),
            "8".into(),
            "--elevated-probe-layer-m".into(),
            "30".into(),
            "--elevated-probe-layer-m".into(),
            "63".into(),
            "--elevated-probe-spacing-m".into(),
            "16".into(),
        ])
        .unwrap();
        assert_eq!(config.probe_spacing_m, 8.0);
        assert_eq!(config.elevated_probe_layers_m, vec![30.0, 63.0]);
        assert_eq!(config.elevated_probe_spacing_m, Some(16.0));
    }

    #[test]
    fn city_bake_rejects_elevated_spacing_without_a_layer() {
        let error = parse_city_bake_args(&[
            "--package".into(),
            "city.fightbox".into(),
            "--output".into(),
            "city.baked".into(),
            "--elevated-probe-spacing-m".into(),
            "16".into(),
        ])
        .unwrap_err();
        assert_eq!(
            error.message(),
            "--elevated-probe-spacing-m requires at least one --elevated-probe-layer-m"
        );
    }

    #[test]
    fn city_bake_rejects_a_repeated_elevated_layer_altitude() {
        let error = parse_city_bake_args(&[
            "--package".into(),
            "city.fightbox".into(),
            "--output".into(),
            "city.baked".into(),
            "--elevated-probe-layer-m".into(),
            "30".into(),
            "--elevated-probe-layer-m".into(),
            "30".into(),
        ])
        .unwrap_err();
        assert!(error.message().contains("duplicate"));
    }

    #[test]
    fn rejects_invalid_city_bake_tuning() {
        let required = [
            "--package".to_string(),
            "city.fightbox".to_string(),
            "--output".to_string(),
            "city.baked".to_string(),
        ];
        for (flag, value) in [
            ("--path-range-m", "0"),
            ("--visibility-range-m", "NaN"),
            ("--visibility-samples", "-1"),
            ("--visibility-threshold", "1.1"),
            ("--probe-spacing-m", "inf"),
            ("--probe-height-above-floor-m", "0"),
            ("--probe-ceiling-m", "-3"),
            ("--elevated-probe-spacing-m", "0"),
            ("--elevated-probe-spacing-m", "-8"),
            ("--elevated-probe-spacing-m", "NaN"),
            ("--bake-threads", "0"),
        ] {
            let mut args = required.to_vec();
            args.extend([flag.to_string(), value.to_string()]);
            assert!(
                parse_city_bake_args(&args).is_err(),
                "{flag}={value} should be rejected"
            );
        }
        let mut duplicate = required.to_vec();
        duplicate.extend(["--path-range-m".into(), "100".into()]);
        duplicate.extend(["--path-range-m".into(), "200".into()]);
        assert!(parse_city_bake_args(&duplicate).is_err());

        let mut duplicate_spacing = required.to_vec();
        duplicate_spacing.extend(["--elevated-probe-spacing-m".into(), "16".into()]);
        duplicate_spacing.extend(["--elevated-probe-spacing-m".into(), "32".into()]);
        let error = parse_city_bake_args(&duplicate_spacing).unwrap_err();
        assert!(error.message().contains("duplicate"));
    }

    #[test]
    fn dispatch_phase_a_sweep_requires_out() {
        let result = dispatch(&["phase-a".to_string(), "sweep".to_string()]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message().contains("--out"));
    }

    #[test]
    fn parse_s0_args_requires_fixture_and_out() {
        assert!(parse_s0_args(&[]).is_err());
        assert!(parse_s0_args(&["--fixture".into(), "x".into()]).is_err());
        let (f, o) =
            parse_s0_args(&["--fixture".into(), "a".into(), "--out".into(), "b".into()]).unwrap();
        assert_eq!(f, PathBuf::from("a"));
        assert_eq!(o, PathBuf::from("b"));
    }

    #[test]
    fn parse_s3_render_args_requires_fixture_world_out() {
        assert!(parse_s3_render_args(&[]).is_err());
        let (f, w, o) = parse_s3_render_args(&[
            "--fixture".into(),
            "a".into(),
            "--world".into(),
            "w".into(),
            "--out".into(),
            "b".into(),
        ])
        .unwrap();
        assert_eq!(f, PathBuf::from("a"));
        assert_eq!(w, PathBuf::from("w"));
        assert_eq!(o, PathBuf::from("b"));
    }

    #[test]
    fn parse_verify_args_handles_mechanical_only_flag() {
        let (b, m) = parse_verify_args(&["--bundle".into(), "x".into()]).unwrap();
        assert_eq!(b, PathBuf::from("x"));
        assert!(!m);
        let (b, m) =
            parse_verify_args(&["--bundle".into(), "x".into(), "--mechanical-only".into()])
                .unwrap();
        assert_eq!(b, PathBuf::from("x"));
        assert!(m);
    }

    #[test]
    fn parse_phase_b_s6a_requires_fixture_and_output() {
        assert!(parse_phase_b_s6a_args(&[]).is_err());
        let (fixture, output, isolation, reflection_effect) = parse_phase_b_s6a_args(&[
            "--fixture".into(),
            "fixture.json".into(),
            "--output".into(),
            "/tmp/s6a".into(),
            "--isolation-check".into(),
        ])
        .unwrap();
        assert_eq!(fixture, PathBuf::from("fixture.json"));
        assert_eq!(output, PathBuf::from("/tmp/s6a"));
        assert!(isolation);
        assert_eq!(reflection_effect, ReflectionEffectConfig::PARAMETRIC);
    }

    #[test]
    fn parse_phase_b_s6a_handles_convolution() {
        let (_, _, _, reflection_effect) = parse_phase_b_s6a_args(&[
            "--fixture".into(),
            "fixture.json".into(),
            "--output".into(),
            "/tmp/s6a".into(),
            "--reflection-effect".into(),
            "convolution".into(),
        ])
        .unwrap();
        assert_eq!(reflection_effect, ReflectionEffectConfig::CONVOLUTION);
    }

    #[test]
    fn parse_phase_b_soak_handles_live() {
        let (minutes, output, live, reflection_effect) = parse_phase_b_soak_args(&[
            "--minutes".into(),
            "30".into(),
            "--output".into(),
            "/tmp/soak".into(),
            "--live".into(),
        ])
        .unwrap();
        assert_eq!(minutes, 30);
        assert_eq!(output, PathBuf::from("/tmp/soak"));
        assert!(live);
        assert_eq!(reflection_effect, ReflectionEffectConfig::CONVOLUTION);
        assert!(parse_phase_b_soak_args(&["--minutes".into(), "nope".into()]).is_err());
    }

    #[test]
    fn parse_phase_b_soak_handles_parametric() {
        let (_, _, _, reflection_effect) = parse_phase_b_soak_args(&[
            "--minutes".into(),
            "30".into(),
            "--output".into(),
            "/tmp/soak".into(),
            "--reflection-effect".into(),
            "parametric".into(),
        ])
        .unwrap();
        assert_eq!(reflection_effect, ReflectionEffectConfig::PARAMETRIC);
    }

    #[test]
    fn parse_phase_b_rejects_unknown_reflection_effect() {
        let error = parse_reflection_effect("hybrid").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid --reflection-effect \"hybrid\"; expected parametric or convolution"
        );
    }
}
