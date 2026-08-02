use std::path::PathBuf;

use fightbox_workbench::{LaunchArgs, launch};

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!(
                "usage: fightbox-workbench --package <pkg.fightbox> --baked <dir> \
                 --fixture <fixture.json> [--fixture <fixture.json> ...] [--device <name>]"
            );
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
    let mut device = None;
    let mut arguments = arguments;
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--package" => package = Some(PathBuf::from(value)),
            "--baked" => baked = Some(PathBuf::from(value)),
            "--fixture" => fixtures.push(PathBuf::from(value)),
            "--device" => device = Some(value),
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    Ok(LaunchArgs {
        package: package.ok_or("--package is required")?,
        baked: baked.ok_or("--baked is required")?,
        fixtures: (!fixtures.is_empty())
            .then_some(fixtures)
            .ok_or("--fixture is required")?,
        device,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "--device",
                "DAC",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.package, PathBuf::from("block.fightbox"));
        assert_eq!(args.fixtures, vec![PathBuf::from("fixture.json")]);
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
    }
}
