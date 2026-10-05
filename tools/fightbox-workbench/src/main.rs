use std::path::PathBuf;

use fightbox_workbench::{LaunchArgs, QuietAuditionOptions, RenderFormat, ReplayOptions, ReplayShotSpot, launch};

const HELP: &str = "usage: fightbox-workbench --package <pkg.fightbox> --baked <dir> \
                 --fixture <fixture.json> [--fixture <fixture.json> ...] \
                 [--start-audio [--device <name>]] [--quiet-audition-seconds <1..60>] \
                 [--headless-replay --seconds <1..120> --capture-root <absolute-dir> [--replay-monitor-gain-db <-20..40>] [--replay-shot <A|B>] [--replay-pin-full-quality]] \
                 [--live-input-wav <file> (test-only: real-time ring feeder, no input device)]\n\
                 --replay-pin-full-quality  Diagnostic Full pin; requires headless null output. Timing and safety stay active.\n\n\
                 --null-output  Run the live output chain without an audio device; excludes --device.\n\
                 --render-out <absolute.wav> --seconds <1..120> [--render-format binaural|ambix]  Render headlessly with --null-output.";

fn main() {
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return;
    }
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{HELP}");
            std::process::exit(2);
        }
    };
    if let Err(error) = launch(args) {
        eprintln!("fightbox-workbench: {error}");
        std::process::exit(1);
    }
}

fn parse_args(arguments: impl Iterator<Item = String>) -> Result<LaunchArgs, String> {
    let mut package = None;
    let mut baked = None;
    let mut fixtures = Vec::new();
    let mut start_audio = false;
    let mut device = None;
    let mut null_output = false;
    let mut render_out = None;
    let mut render_format = None;
    let mut live_input_wav = None;
    let mut program_file = None;
    let mut headless = false;
    let mut pin_full_quality = false;
    let mut seconds = None;
    let mut capture_root = None;
    let mut quiet_seconds = None;
    let mut replay_monitor_gain_db = None;
    let mut shot_spot = None;
    let mut arguments = arguments;
    while let Some(flag) = arguments.next() {
        if flag == "--replay-pin-full-quality" {
            pin_full_quality = true;
            continue;
        }
        if flag == "--headless-replay" {
            headless = true;
            continue;
        }
        if flag == "--start-audio" {
            start_audio = true;
            continue;
        }
        if flag == "--null-output" {
            null_output = true;
            continue;
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--package" => package = Some(PathBuf::from(value)),
            "--baked" => baked = Some(PathBuf::from(value)),
            "--fixture" => fixtures.push(PathBuf::from(value)),
            "--device" => device = Some(value),
            "--render-out" => render_out = Some(PathBuf::from(value)),
            "--render-format" => {
                if render_format.is_some() {
                    return Err("--render-format may be supplied only once".into());
                }
                render_format = Some(match value.as_str() {
                    "binaural" => RenderFormat::Binaural,
                    "ambix" => RenderFormat::Ambix,
                    _ => return Err("--render-format must be binaural or ambix".into()),
                });
            }
            "--live-input-wav" => live_input_wav = Some(PathBuf::from(value)),
            "--program-file" => program_file = Some(PathBuf::from(value)),
            "--seconds" => {
                seconds = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| "--seconds must be an integer in 1..=120")?,
                )
            }
            "--capture-root" => capture_root = Some(PathBuf::from(value)),
            "--replay-monitor-gain-db" => {
                if replay_monitor_gain_db.is_some() {
                    return Err("--replay-monitor-gain-db may be supplied only once".into());
                }
                replay_monitor_gain_db = Some(
                    value
                        .parse::<i32>()
                        .ok()
                        .filter(|gain| (-20..=40).contains(gain))
                        .ok_or("--replay-monitor-gain-db must be an integer in -20..=40")?,
                );
            }
            "--replay-shot" => {
                shot_spot = Some(match value.as_str() {
                    "A" => ReplayShotSpot::A,
                    "B" => ReplayShotSpot::B,
                    _ => return Err("--replay-shot must be A or B".into()),
                });
            }
            "--quiet-audition-seconds" => {
                if quiet_seconds.is_some() {
                    return Err("--quiet-audition-seconds may be supplied only once".into());
                }
                quiet_seconds = Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| (1..=60).contains(n))
                        .ok_or("--quiet-audition-seconds must be in 1..=60")?,
                );
            }
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    if render_format.is_some() && render_out.is_none() {
        return Err("--render-format requires --render-out".into());
    }
    if render_format == Some(RenderFormat::Ambix) && replay_monitor_gain_db.is_some_and(|gain| gain != 0) {
        return Err("AmbiX export requires calibrated monitor gain 0 dB".into());
    }
    if render_out.is_some() {
        if capture_root.is_some() {
            return Err("--render-out and --capture-root are mutually exclusive".into());
        }
        null_output = true;
        headless = true;
    }
    if pin_full_quality && (!headless || !null_output) {
        return Err("--replay-pin-full-quality requires --headless-replay and --null-output".into());
    }
    if null_output {
        if device.is_some() {
            return Err("--null-output and --device are mutually exclusive".into());
        }
        start_audio = true;
    }
    if device.is_some() && !start_audio {
        return Err("--device requires explicit --start-audio".into());
    }
    if live_input_wav.is_some() && !start_audio {
        return Err("--live-input-wav requires --start-audio".into());
    }
    if program_file.is_some() && (!headless || live_input_wav.is_some()) {
        return Err("--program-file requires headless replay and excludes --live-input-wav".into());
    }
    let quiet_audition = quiet_seconds.map(|seconds| QuietAuditionOptions { seconds });
    if quiet_audition.is_some()
        && (!start_audio || device.as_deref().is_none_or(|name| name.trim().is_empty()))
    {
        return Err("quiet audition requires --start-audio and an explicit --device".into());
    }
    if replay_monitor_gain_db.is_some() && !headless {
        return Err("--replay-monitor-gain-db requires --headless-replay".into());
    }
    if shot_spot.is_some() && !headless {
        return Err("--replay-shot requires --headless-replay".into());
    }
    let replay = if headless {
        if !start_audio
            || (!null_output && device.as_deref().is_none_or(|name| name.trim().is_empty()))
        {
            return Err(
                "headless replay requires --null-output or --start-audio and an explicit --device"
                    .into(),
            );
        }
        if fixtures.len() != 1 {
            return Err("headless replay requires exactly one fixture".into());
        }
        let seconds = seconds
            .filter(|n| (1..=120).contains(n))
            .ok_or("--seconds must be in 1..=120")?;
        let capture_root = if let Some(path) = render_out.as_ref() {
            if !path.is_absolute() || path.file_name().is_none() {
                return Err("--render-out must be an absolute file path".into());
            }
            path.parent().unwrap().to_path_buf()
        } else {
            capture_root
                .filter(|p| p.is_absolute())
                .ok_or("--capture-root must be absolute")?
        };
        Some(ReplayOptions {
            seconds,
            pin_full_quality,
            monitor_gain_db: replay_monitor_gain_db.unwrap_or(0),
            capture_root,
            shot_spot,
        })
    } else {
        if seconds.is_some() || capture_root.is_some() {
            return Err("--seconds and --capture-root require --headless-replay".into());
        }
        None
    };
    Ok(LaunchArgs {
        package: package.ok_or("--package is required")?,
        baked: baked.ok_or("--baked is required")?,
        fixtures: (!fixtures.is_empty())
            .then_some(fixtures)
            .ok_or("--fixture is required")?,
        start_audio,
        device,
        null_output,
        render_out,
        render_format: render_format.unwrap_or_default(),
        live_input_wav,
        program_file,
        replay,
        quiet_audition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_quality_pin_is_replay_only_and_opt_in() {
        let base = ["--package", "p", "--baked", "b", "--fixture", "f"];
        let replay = ["--headless-replay", "--null-output", "--seconds", "32", "--capture-root", "/tmp/replay"];
        let parse = |extra: Vec<&str>| parse_args(base.into_iter().chain(extra).map(str::to_owned));
        assert!(!parse(replay.to_vec()).unwrap().replay.unwrap().pin_full_quality);
        assert!(parse(replay.into_iter().chain(["--replay-pin-full-quality"]).collect()).unwrap().replay.unwrap().pin_full_quality);
        assert!(parse(vec!["--replay-pin-full-quality"]).is_err());
        assert!(parse(vec!["--headless-replay", "--start-audio", "--device", "DAC", "--replay-pin-full-quality"]).is_err());
    }

    #[test]
    fn parses_required_paths_and_optional_device() {
        let args = parse_args(
            [
                "--package",
                "block.fightbox",
                "--baked",
                "bake",
                "--fixture",
                "fixture.json",
                "--start-audio",
                "--device",
                "DAC",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.package, PathBuf::from("block.fightbox"));
        assert_eq!(args.fixtures, vec![PathBuf::from("fixture.json")]);
        assert!(args.start_audio);
        assert_eq!(args.device.as_deref(), Some("DAC"));
    }

    #[test]
    fn preserves_repeated_fixture_order_for_scene_tabs() {
        let args = parse_args(
            [
                "--package",
                "block.fightbox",
                "--baked",
                "bake",
                "--fixture",
                "megablock.json",
                "--fixture",
                "checkpoint.json",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            args.fixtures,
            vec![
                PathBuf::from("megablock.json"),
                PathBuf::from("checkpoint.json")
            ]
        );
        assert!(!args.start_audio);
    }

    #[test]
    fn device_selection_requires_explicit_audio_start() {
        let error = parse_args(
            [
                "--package",
                "block.fightbox",
                "--baked",
                "bake",
                "--fixture",
                "fixture.json",
                "--device",
                "DAC",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap_err();
        assert_eq!(error, "--device requires explicit --start-audio");
    }

    #[test]
    fn null_output_starts_audio_without_a_device_and_rejects_device_selection() {
        let base = ["--package", "p", "--baked", "b", "--fixture", "f"];
        let args =
            parse_args(base.into_iter().chain(["--null-output"]).map(str::to_owned)).unwrap();
        assert!(args.null_output && args.start_audio);
        assert!(args.device.is_none());
        assert!(
            parse_args(
                base.into_iter()
                    .chain(["--null-output", "--device", "DAC"])
                    .map(str::to_owned)
            )
            .is_err()
        );
        let args = parse_args(
            base.into_iter()
                .chain([
                    "--null-output",
                    "--headless-replay",
                    "--seconds",
                    "6",
                    "--capture-root",
                    "/tmp/capture",
                ])
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.replay.unwrap().seconds, 6);
    }

    #[test]
    fn render_output_implies_null_replay_and_requires_one_absolute_destination() {
        let base = ["--package", "p", "--baked", "b", "--fixture", "f"];
        let render = ["--render-out", "/tmp/street-A.wav", "--seconds", "8"];
        let args = parse_args(
            base.into_iter()
                .chain(render)
                .chain(["--replay-monitor-gain-db", "15", "--replay-shot", "A"])
                .map(str::to_owned),
        )
        .unwrap();
        assert!(args.null_output && args.start_audio);
        assert_eq!(args.render_out, Some(PathBuf::from("/tmp/street-A.wav")));
        let replay = args.replay.unwrap();
        assert_eq!(replay.capture_root, PathBuf::from("/tmp"));
        assert_eq!(replay.seconds, 8);
        assert_eq!(replay.monitor_gain_db, 15);
        assert_eq!(replay.shot_spot, Some(ReplayShotSpot::A));
        for extra in [
            vec!["--device", "DAC"],
            vec!["--capture-root", "/tmp/capture"],
            vec!["--fixture", "second"],
            vec!["--seconds", "0"],
        ] {
            assert!(
                parse_args(
                    base.into_iter()
                        .chain(render)
                        .chain(extra)
                        .map(str::to_owned)
                )
                .is_err()
            );
        }
        for path in ["relative.wav", "/"] {
            assert!(
                parse_args(
                    base.into_iter()
                        .chain(["--render-out", path, "--seconds", "8"])
                        .map(str::to_owned)
                )
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod replay_cli_tests {
    use super::*;

    fn replay(extra: &[&str]) -> Result<LaunchArgs, String> {
        parse_args(
            [
                "--package",
                "p",
                "--baked",
                "b",
                "--fixture",
                "f",
                "--headless-replay",
            ]
            .into_iter()
            .chain(extra.iter().copied())
            .map(str::to_owned),
        )
    }

    #[test]
    fn replay_monitor_matches_live_thirty_and_preserves_default_zero() {
        let base = [
            "--start-audio",
            "--device",
            "BlackHole 2ch",
            "--seconds",
            "8",
            "--capture-root",
            "/tmp/capture",
        ];
        assert_eq!(replay(&base).unwrap().replay.unwrap().monitor_gain_db, 0);
        for gain in ["-20", "0", "30", "40"] {
            let args = base
                .into_iter()
                .chain(["--replay-monitor-gain-db", gain])
                .collect::<Vec<_>>();
            assert_eq!(
                replay(&args).unwrap().replay.unwrap().monitor_gain_db,
                gain.parse::<i32>().unwrap()
            );
        }
        for gain in ["-21", "41", "NaN", "30.5"] {
            let args = base
                .into_iter()
                .chain(["--replay-monitor-gain-db", gain])
                .collect::<Vec<_>>();
            assert!(replay(&args).is_err());
        }
        assert!(
            parse_args(
                [
                    "--package",
                    "p",
                    "--baked",
                    "b",
                    "--fixture",
                    "f",
                    "--replay-monitor-gain-db",
                    "30"
                ]
                .into_iter()
                .map(str::to_owned)
            )
            .is_err()
        );
    }

    #[test]
    fn replay_fails_closed_without_explicit_device_or_bounded_duration() {
        assert!(
            replay(&[
                "--start-audio",
                "--seconds",
                "20",
                "--capture-root",
                "/tmp/capture"
            ])
            .is_err()
        );
        for seconds in ["0", "121", "NaN", "-1"] {
            assert!(
                replay(&[
                    "--start-audio",
                    "--device",
                    "BlackHole 2ch",
                    "--seconds",
                    seconds,
                    "--capture-root",
                    "/tmp/capture"
                ])
                .is_err()
            );
        }
        assert!(
            replay(&[
                "--start-audio",
                "--device",
                "BlackHole 2ch",
                "--seconds",
                "20",
                "--capture-root",
                "/tmp/capture",
                "--fixture",
                "f2"
            ])
            .is_err()
        );
        let args = replay(&[
            "--start-audio",
            "--device",
            "BlackHole 2ch",
            "--seconds",
            "20",
            "--capture-root",
            "/tmp/capture",
        ])
        .unwrap();
        assert_eq!(args.replay.unwrap().seconds, 20);
    }
}

#[cfg(test)]
mod quiet_cli_tests {
    use super::*;

    fn parse(extra: &[&str]) -> Result<LaunchArgs, String> {
        parse_args(
            ["--package", "p", "--baked", "b", "--fixture", "f"]
                .into_iter()
                .chain(extra.iter().copied())
                .map(str::to_owned),
        )
    }

    #[test]
    fn quiet_audition_requires_bounded_duration_explicit_start_and_named_device() {
        assert!(parse(&[]).unwrap().quiet_audition.is_none());
        for duration in ["0", "61", "-1", "NaN", "1.5"] {
            assert!(
                parse(&[
                    "--start-audio",
                    "--device",
                    "BlackHole 2ch",
                    "--quiet-audition-seconds",
                    duration
                ])
                .is_err()
            );
        }
        assert!(parse(&["--quiet-audition-seconds", "10"]).is_err());
        assert!(parse(&["--start-audio", "--quiet-audition-seconds", "10"]).is_err());
        assert!(
            parse(&[
                "--start-audio",
                "--device",
                " ",
                "--quiet-audition-seconds",
                "10"
            ])
            .is_err()
        );
        let args = parse(&[
            "--start-audio",
            "--device",
            "BlackHole 2ch",
            "--quiet-audition-seconds",
            "10",
        ])
        .unwrap();
        assert_eq!(args.quiet_audition.unwrap().seconds, 10);
        assert_eq!(args.device.as_deref(), Some("BlackHole 2ch"));
    }
}
