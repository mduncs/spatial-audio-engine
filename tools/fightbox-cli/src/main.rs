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

mod asset;
mod atomicio;
mod bundle;
mod calibrate;
mod city;
mod error;
mod fixture;
mod metrics;
mod phase_b;
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
phase-b soak     Run the four-source offline or feature-gated live soak\n    \
                  [--reflection-effect <parametric|convolution>] (default: convolution)\n\n\
city compile     Compile GeoJSON into a deterministic .fightbox package\n    \
city synth       Generate a deterministic Manhattan-style GeoJSON city\n    \
city inspect     Print a package manifest summary and assumptions\n    \
city export-obj  Export a package mesh as deterministic triangulated OBJ\n    \
city bake        Bake probes for a city package (requires linked-sdk)\n    \
city render      Render a fixture through a packaged and baked city\n\n\
city metamorphic Jitter assumed heights, bake, and assert the occlusion percept\n\n\
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
        Some("city") => dispatch_city(&args[1..]),
        Some(command) => Err(error::CliError::new(format!(
            "unknown command: {command}\n\n{HELP}"
        ))),
    }
}

fn dispatch_city(args: &[String]) -> error::Result<()> {
    match args.first().map(String::as_str) {
        Some("synth") => {
            let (seed, blocks, output) = parse_city_synth_args(&args[1..])?;
            city::synth(seed, blocks, &output)
        }
        Some("compile") => {
            let values = parse_named_paths(&args[1..], &["--geojson", "--output"])?;
            city::compile_geojson(&values[0], &values[1])
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
            let values = parse_named_paths(&args[1..], &["--package", "--output"])?;
            city::bake(&values[0], &values[1])
        }
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
        Some("soak") => {
            let (minutes, output, live, reflection_effect) = parse_phase_b_soak_args(&args[1..])?;
            phase_b::run_soak(minutes, &output, live, reflection_effect)
        }
        Some(sub) => Err(error::CliError::new(format!(
            "unknown phase-b subcommand: {sub}\n\n{HELP}"
        ))),
        None => Err(error::CliError::new(format!(
            "phase-b requires a subcommand\n\n{HELP}"
        ))),
    }
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
    }

    #[test]
    fn dispatch_help_with_no_args() {
        // No args should print help and succeed.
        assert!(dispatch(&[]).is_ok());
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
