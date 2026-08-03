//! Offline cheap proxy-field runner. It never constructs render effects or runs reflections.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fightbox_api::{Directivity, ExtentDescriptor, ReferenceLevel};
use fightbox_steam_audio::{
    AnomalyClass, AnomalyQuerySession, DirectOcclusionMode, GridSpec, MultiSourceDescriptor,
    ProxyCell, S3SimulationConfig, classify_grid, classify_sample_at_distance,
};
use fightbox_world::read_package;
use serde::Serialize;
use serde_json::Value;

use crate::atomicio::{AtomicDir, validate_output_path, write_bytes_atomic, write_json_atomic};
use crate::error::{CliError, Result};

const DEFAULT_SPACING_M: f32 = 8.0;
const DEFAULT_LISTENER_HEIGHT_M: f32 = 1.5;
const RASTER_MAGIC: &[u8; 8] = b"FBXANOM\0";
const SOURCE_COVERED_BIT: u32 = 1 << 31;
const LISTENER_COVERED_BIT: u32 = 1 << 30;
const ANOMALY_BITS: u32 = (1 << AnomalyClass::ALL.len()) - 1;

#[derive(Clone, Debug)]
struct Args {
    package: PathBuf,
    baked: PathBuf,
    fixture: PathBuf,
    source_id: String,
    source_height_m: Option<f32>,
    listener_height_m: f32,
    spacing_m: f32,
    inspect_position: Option<[f32; 3]>,
    output: PathBuf,
}

pub(crate) fn run(arguments: &[String]) -> Result<()> {
    match arguments.first().map(String::as_str) {
        Some("sweep") => sweep(parse_args(&arguments[1..])?),
        Some(other) => Err(CliError::new(format!(
            "unknown anomaly-field subcommand {other:?}; expected sweep"
        ))),
        None => Err(CliError::new("anomaly-field requires the sweep subcommand")),
    }
}

fn parse_args(arguments: &[String]) -> Result<Args> {
    let mut package = None;
    let mut baked = None;
    let mut fixture = None;
    let mut source_id = None;
    let mut source_height_m = None;
    let mut listener_height_m = None;
    let mut spacing_m = None;
    let mut inspect_position = None;
    let mut output = None;
    let mut iter = arguments.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| CliError::new(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--package" => set_once(&mut package, PathBuf::from(value), flag)?,
            "--baked" => set_once(&mut baked, PathBuf::from(value), flag)?,
            "--fixture" => set_once(&mut fixture, PathBuf::from(value), flag)?,
            "--source" => set_once(&mut source_id, value.clone(), flag)?,
            "--source-height-m" => set_once(&mut source_height_m, finite_f32(value, flag)?, flag)?,
            "--listener-height-m" => {
                set_once(&mut listener_height_m, finite_f32(value, flag)?, flag)?
            }
            "--spacing-m" => {
                let spacing = finite_f32(value, flag)?;
                if spacing <= 0.0 {
                    return Err(CliError::new("--spacing-m must be positive"));
                }
                set_once(&mut spacing_m, spacing, flag)?;
            }
            "--inspect-position" => {
                set_once(&mut inspect_position, parse_position(value, flag)?, flag)?
            }
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            other => {
                return Err(CliError::new(format!(
                    "unknown anomaly-field argument {other:?}"
                )));
            }
        }
    }
    Ok(Args {
        package: package.ok_or_else(|| CliError::new("missing required --package <path>"))?,
        baked: baked.ok_or_else(|| CliError::new("missing required --baked <path>"))?,
        fixture: fixture.ok_or_else(|| CliError::new("missing required --fixture <path>"))?,
        source_id: source_id.ok_or_else(|| CliError::new("missing required --source <id>"))?,
        source_height_m,
        listener_height_m: listener_height_m.unwrap_or(DEFAULT_LISTENER_HEIGHT_M),
        spacing_m: spacing_m.unwrap_or(DEFAULT_SPACING_M),
        inspect_position,
        output: output.ok_or_else(|| CliError::new("missing required --output <path>"))?,
    })
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        Err(CliError::new(format!("duplicate argument {flag}")))
    } else {
        Ok(())
    }
}

fn finite_f32(value: &str, flag: &str) -> Result<f32> {
    let parsed = value
        .parse::<f32>()
        .map_err(|_| CliError::new(format!("{flag} requires a finite number")))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(CliError::new(format!("{flag} requires a finite number")))
    }
}

fn parse_position(value: &str, flag: &str) -> Result<[f32; 3]> {
    let values = value
        .split(',')
        .map(|part| finite_f32(part, flag))
        .collect::<Result<Vec<_>>>()?;
    values.try_into().map_err(|_| {
        CliError::new(format!(
            "{flag} requires three comma-separated east,north,up numbers"
        ))
    })
}

#[derive(Clone, Debug)]
struct FixtureQuery {
    source_position: fightbox_api::EnuVector3,
    source_spl_db: f32,
    descriptor: MultiSourceDescriptor,
    simulation: S3SimulationConfig,
    bounds: [[f32; 2]; 2],
}

fn read_fixture(
    path: &Path,
    source_id: &str,
    source_height_m: Option<f32>,
) -> Result<FixtureQuery> {
    let bytes = std::fs::read(path).map_err(|error| {
        CliError::new(format!("cannot read fixture {}: {error}", path.display()))
    })?;
    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::new(format!("invalid fixture {}: {error}", path.display())))?;
    let sources = root
        .get("sources")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::new("fixture requires a sources array"))?;
    let source = sources
        .iter()
        .find(|source| source.get("id").and_then(Value::as_str) == Some(source_id))
        .ok_or_else(|| CliError::new(format!("fixture has no source {source_id:?}")))?;
    let mut position = value_vec3(source.get("position_m"), "source.position_m")?;
    if let Some(height) = source_height_m {
        position[2] = height;
    }
    let source_position = fightbox_api::EnuVector3::new(position[0], position[1], position[2]);
    let source_spl_db = value_f32(
        source.pointer("/reference_level/db_spl"),
        "source.reference_level.db_spl",
    )?;
    let directivity = Directivity {
        dipole_weight: optional_f32(source.pointer("/directivity/dipole_weight")).unwrap_or(0.0),
        dipole_power: optional_f32(source.pointer("/directivity/dipole_power")).unwrap_or(1.0),
    };
    directivity
        .validate()
        .map_err(|error| CliError::new(format!("invalid source directivity: {error:?}")))?;
    let extent = parse_extent(source.get("extent"))?;
    extent
        .validate()
        .map_err(|error| CliError::new(format!("invalid source extent: {error:?}")))?;
    let descriptor = MultiSourceDescriptor::at(source_position)
        .with_reference_level(ReferenceLevel::SplAtOneMeter {
            db_spl: source_spl_db,
        })
        .with_directivity(directivity)
        .with_extent(extent);

    let probe_min = value_vec3(
        root.pointer("/simulation/probe_volume/min_m"),
        "simulation.probe_volume.min_m",
    )?;
    let probe_max = value_vec3(
        root.pointer("/simulation/probe_volume/max_m"),
        "simulation.probe_volume.max_m",
    )?;
    let probe_spacing = value_f32(
        root.pointer("/simulation/probe_volume/spacing_m"),
        "simulation.probe_volume.spacing_m",
    )?;
    let mut simulation = S3SimulationConfig::default();
    simulation.max_occlusion_samples =
        optional_i32(root.pointer("/simulation/direct/occlusion_samples")).unwrap_or(64);
    simulation.direct_occlusion = DirectOcclusionMode::Raycast;
    simulation.pathing_order = optional_i32(root.pointer("/simulation/pathing/order")).unwrap_or(2);
    simulation.pathing_visibility_range_m =
        optional_f32(root.pointer("/simulation/pathing/visibility_range_m"))
            .unwrap_or(S3SimulationConfig::default().pathing_visibility_range_m)
            .max(probe_spacing * 2.5);
    simulation.validate_paths = root
        .pointer("/simulation/pathing/validation")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    simulation.find_alternate_paths = root
        .pointer("/simulation/pathing/alternate_paths")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    simulation.simulation_threads = 1;
    Ok(FixtureQuery {
        source_position,
        source_spl_db,
        descriptor,
        simulation,
        bounds: [[probe_min[0], probe_min[1]], [probe_max[0], probe_max[1]]],
    })
}

fn parse_extent(value: Option<&Value>) -> Result<ExtentDescriptor> {
    let Some(value) = value else {
        return Ok(ExtentDescriptor::Point);
    };
    match value.get("kind").and_then(Value::as_str) {
        Some("point") => Ok(ExtentDescriptor::Point),
        Some("multi_point") => Ok(ExtentDescriptor::MultiPoint {
            count: value
                .get("count")
                .and_then(Value::as_u64)
                .and_then(|count| u8::try_from(count).ok())
                .ok_or_else(|| CliError::new("multi_point extent requires a u8 count"))?,
        }),
        Some("line_segment") => Ok(ExtentDescriptor::LineSegment {
            length_m: value_f32(value.get("length_m"), "extent.length_m")?,
        }),
        Some("stereo_image") => Ok(ExtentDescriptor::StereoImage {
            width_m: value_f32(value.get("width_m"), "extent.width_m")?,
        }),
        other => Err(CliError::new(format!(
            "unsupported source extent kind {other:?}"
        ))),
    }
}

fn value_vec3(value: Option<&Value>, label: &str) -> Result<[f32; 3]> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::new(format!("{label} must be a three-number array")))?;
    if values.len() != 3 {
        return Err(CliError::new(format!(
            "{label} must contain exactly three numbers"
        )));
    }
    let mut output = [0.0; 3];
    for (index, value) in values.iter().enumerate() {
        output[index] = value_f32(Some(value), label)?;
    }
    Ok(output)
}

fn value_f32(value: Option<&Value>, label: &str) -> Result<f32> {
    optional_f32(value).ok_or_else(|| CliError::new(format!("{label} must be a finite number")))
}

fn optional_f32(value: Option<&Value>) -> Option<f32> {
    let value = value?.as_f64()? as f32;
    value.is_finite().then_some(value)
}

fn optional_i32(value: Option<&Value>) -> Option<i32> {
    value?.as_i64().and_then(|value| i32::try_from(value).ok())
}

#[derive(Serialize)]
struct Report {
    schema_version: &'static str,
    metric: &'static str,
    package: String,
    baked: String,
    fixture: String,
    source_id: String,
    source_position_enu: [f32; 3],
    source_spl_at_one_meter_db: f32,
    grid: GridReport,
    timings: TimingReport,
    anomaly_counts: BTreeMap<&'static str, usize>,
    thresholds: Vec<ThresholdReport>,
    top_zones: Vec<CellReport>,
    inspected_position: Option<InspectedPosition>,
    raster_file: &'static str,
    non_claims: [&'static str; 2],
}

#[derive(Serialize)]
struct GridReport {
    min_enu: [f32; 2],
    max_enu: [f32; 2],
    listener_height_m: f32,
    spacing_m: f32,
    width: usize,
    height: usize,
    cell_count: usize,
}

#[derive(Serialize)]
struct TimingReport {
    load_and_session_seconds: f64,
    query_seconds: f64,
    write_seconds: f64,
    total_seconds: f64,
}

#[derive(Serialize)]
struct ThresholdReport {
    id: &'static str,
    threshold: &'static str,
    rationale: &'static str,
}

#[derive(Serialize)]
struct CellReport {
    position_enu: [f32; 3],
    direct_audibility: f32,
    direct_loss_db: f32,
    path_sh_energy: f32,
    path_strength_db: f32,
    free_field_db: f32,
    score: f32,
    source_probe_covered: bool,
    listener_probe_covered: bool,
    classes: Vec<&'static str>,
}

#[derive(Serialize)]
struct InspectedPosition {
    requested_enu: [f32; 3],
    exact: CellReport,
    nearest_grid: CellReport,
    exact_flagged: bool,
    nearest_grid_flagged: bool,
}

fn sweep(args: Args) -> Result<()> {
    let total_started = Instant::now();
    let output = validate_output_path(&args.output)?;
    let fixture = read_fixture(&args.fixture, &args.source_id, args.source_height_m)?;
    let loaded = read_package(&args.package)
        .map_err(|error| CliError::new(format!("cannot load package: {error}")))?;
    let baked = crate::city::load_baked(&args.baked)?;
    crate::city::verify_bake_identity(&loaded, &args.baked, &baked)?;
    let scene = crate::city::scene_mesh(&loaded)?;
    let mut query =
        AnomalyQuerySession::new(&scene, &baked, fixture.simulation, fixture.descriptor).map_err(
            |error| CliError::new(format!("cannot build anomaly query session: {error}")),
        )?;
    let load_and_session_seconds = total_started.elapsed().as_secs_f64();
    let grid = GridSpec {
        min_enu: fixture.bounds[0],
        max_enu: fixture.bounds[1],
        listener_height_m: args.listener_height_m,
        spacing_m: args.spacing_m,
    }
    .validate()
    .map_err(CliError::new)?;

    let query_started = Instant::now();
    let mut cells = Vec::with_capacity(grid.cell_count());
    let progress_stride = grid.width().saturating_mul(8).max(1);
    for index in 0..grid.cell_count() {
        let position = grid.position(index);
        let raw = query.sample(position).map_err(|error| {
            CliError::new(format!(
                "anomaly query failed at cell {index} {position:?}: {error}"
            ))
        })?;
        cells.push(classify_sample_at_distance(
            raw,
            fixture.source_spl_db,
            distance(fixture.source_position, position),
        ));
        if (index + 1).is_multiple_of(progress_stride) || index + 1 == grid.cell_count() {
            eprintln!(
                "fightbox: anomaly field {}/{} cells ({:.0}%)",
                index + 1,
                grid.cell_count(),
                100.0 * (index + 1) as f32 / grid.cell_count() as f32
            );
        }
    }
    classify_grid(&mut cells, grid.width(), grid.height(), grid.spacing_m);
    let inspected_position = args
        .inspect_position
        .map(|requested| inspect_position(&mut query, &fixture, requested, &cells))
        .transpose()?;
    let query_seconds = query_started.elapsed().as_secs_f64();
    let anomaly_counts = AnomalyClass::ALL
        .into_iter()
        .map(|class| {
            (
                class.id(),
                cells
                    .iter()
                    .filter(|cell| cell.flags.contains(class))
                    .count(),
            )
        })
        .collect();
    let top_zones = top_zones(&cells, grid.spacing_m)
        .into_iter()
        .map(cell_report)
        .collect();

    let directory = AtomicDir::create(output)?;
    let write_started = Instant::now();
    write_bytes_atomic(
        &directory.temp_path().join("cells.bin"),
        &encode_raster(grid, &cells),
    )?;
    let mut report = Report {
        schema_version: "fightbox.anomaly-field.v1",
        metric: "shadow_weak_path_proxy",
        package: args.package.display().to_string(),
        baked: args.baked.display().to_string(),
        fixture: args.fixture.display().to_string(),
        source_id: args.source_id,
        source_position_enu: [
            fixture.source_position.east_m,
            fixture.source_position.north_m,
            fixture.source_position.up_m,
        ],
        source_spl_at_one_meter_db: fixture.source_spl_db,
        grid: GridReport {
            min_enu: grid.min_enu,
            max_enu: grid.max_enu,
            listener_height_m: grid.listener_height_m,
            spacing_m: grid.spacing_m,
            width: grid.width(),
            height: grid.height(),
            cell_count: grid.cell_count(),
        },
        timings: TimingReport {
            load_and_session_seconds,
            query_seconds,
            write_seconds: 0.0,
            total_seconds: 0.0,
        },
        anomaly_counts,
        thresholds: AnomalyClass::ALL
            .into_iter()
            .map(|class| ThresholdReport {
                id: class.id(),
                threshold: class.threshold(),
                rationale: class.rationale(),
            })
            .collect(),
        top_zones,
        inspected_position,
        raster_file: "cells.bin",
        non_claims: [
            "This proxy does not run reflections or predict reflection inversion.",
            "The score omits HRTF, effects, smoothing, monitor gain, and limiting.",
        ],
    };
    report.timings.write_seconds = write_started.elapsed().as_secs_f64();
    report.timings.total_seconds = total_started.elapsed().as_secs_f64();
    write_json_atomic(&directory.temp_path().join("manifest.json"), &report)?;
    directory.commit()?;
    println!(
        "{}",
        serde_json::to_string(&report)
            .map_err(|error| CliError::new(format!("cannot serialize sweep summary: {error}")))?
    );
    Ok(())
}

fn inspect_position(
    query: &mut AnomalyQuerySession,
    fixture: &FixtureQuery,
    requested: [f32; 3],
    cells: &[ProxyCell],
) -> Result<InspectedPosition> {
    let position = fightbox_steam_audio::EnuVector3::new(requested[0], requested[1], requested[2]);
    let exact = classify_sample_at_distance(
        query
            .sample(position)
            .map_err(|error| CliError::new(format!("inspection query failed: {error}")))?,
        fixture.source_spl_db,
        distance(fixture.source_position, position),
    );
    let nearest = cells
        .iter()
        .min_by(|left, right| {
            distance_squared(left.position_enu, position)
                .total_cmp(&distance_squared(right.position_enu, position))
        })
        .copied()
        .ok_or_else(|| CliError::new("cannot inspect an empty field"))?;
    Ok(InspectedPosition {
        requested_enu: requested,
        exact: cell_report(exact),
        nearest_grid: cell_report(nearest),
        exact_flagged: !exact.flags.is_empty(),
        nearest_grid_flagged: !nearest.flags.is_empty(),
    })
}

fn distance(left: fightbox_api::EnuVector3, right: fightbox_steam_audio::EnuVector3) -> f32 {
    let dx = left.east_m - right.x;
    let dy = left.north_m - right.y;
    let dz = left.up_m - right.z;
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn distance_squared(
    left: fightbox_steam_audio::EnuVector3,
    right: fightbox_steam_audio::EnuVector3,
) -> f32 {
    let dx = left.x - right.x;
    let dy = left.y - right.y;
    let dz = left.z - right.z;
    dx * dx + dy * dy + dz * dz
}

fn top_zones(cells: &[ProxyCell], spacing_m: f32) -> Vec<ProxyCell> {
    let mut ranked = cells
        .iter()
        .copied()
        .filter(|cell| !cell.flags.is_empty())
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| severity(*right).total_cmp(&severity(*left)));
    let minimum_zone_distance_squared = (spacing_m * 1.5).powi(2);
    let mut zones: Vec<ProxyCell> = Vec::new();
    for cell in ranked {
        if zones.iter().all(|zone| {
            distance_squared(zone.position_enu, cell.position_enu) >= minimum_zone_distance_squared
        }) {
            zones.push(cell);
            if zones.len() == 10 {
                break;
            }
        }
    }
    zones
}

fn severity(cell: ProxyCell) -> f32 {
    let computation_count = [
        AnomalyClass::InvalidEnergy,
        AnomalyClass::InvalidCoefficient,
        AnomalyClass::ZeroPathWithCoverage,
        AnomalyClass::ReflectionEnergyExcess,
        AnomalyClass::NeighborSpike,
        AnomalyClass::ExcessiveDiscontinuity,
    ]
    .into_iter()
    .filter(|class| cell.flags.contains(*class))
    .count() as f32;
    computation_count * 10.0 + cell.score
}

fn cell_report(cell: ProxyCell) -> CellReport {
    CellReport {
        position_enu: [
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
        classes: AnomalyClass::ALL
            .into_iter()
            .filter(|class| cell.flags.contains(*class))
            .map(AnomalyClass::id)
            .collect(),
    }
}

fn encode_raster(grid: GridSpec, cells: &[ProxyCell]) -> Vec<u8> {
    let bytes_per_cell = 9 * size_of::<f32>() + size_of::<u32>();
    let mut bytes = Vec::with_capacity(8 + 16 + cells.len() * bytes_per_cell);
    bytes.extend_from_slice(RASTER_MAGIC);
    bytes.extend_from_slice(&(grid.width() as u32).to_le_bytes());
    bytes.extend_from_slice(&(grid.height() as u32).to_le_bytes());
    bytes.extend_from_slice(&grid.spacing_m.to_le_bytes());
    bytes.extend_from_slice(&grid.listener_height_m.to_le_bytes());
    for cell in cells {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_configurable_grid_and_inspection_position() {
        let args = parse_args(&[
            "--package".into(),
            "a.fightbox".into(),
            "--baked".into(),
            "a.baked".into(),
            "--fixture".into(),
            "fixture.json".into(),
            "--source".into(),
            "music".into(),
            "--spacing-m".into(),
            "4".into(),
            "--inspect-position".into(),
            "108.06,303.91,1.5".into(),
            "--output".into(),
            "/tmp/field".into(),
        ])
        .unwrap();
        assert_eq!(args.spacing_m, 4.0);
        assert_eq!(args.inspect_position, Some([108.06, 303.91, 1.5]));
    }

    #[test]
    fn raster_header_and_cell_stride_are_stable() {
        let grid = GridSpec {
            min_enu: [0.0, 0.0],
            max_enu: [8.0, 8.0],
            listener_height_m: 1.5,
            spacing_m: 8.0,
        };
        let cell = ProxyCell {
            position_enu: fightbox_steam_audio::EnuVector3::new(4.0, 4.0, 1.5),
            direct_audibility: 1.0,
            direct_loss_db: 0.0,
            path_sh_energy: 0.1,
            path_strength_db: -10.0,
            free_field_db: 80.0,
            score: 0.0,
            source_probe_covered: true,
            listener_probe_covered: true,
            direct_path_energy: None,
            reflection_energy: None,
            reflection_excess_db: None,
            flags: Default::default(),
        };
        let bytes = encode_raster(grid, &[cell]);
        assert_eq!(&bytes[..8], RASTER_MAGIC);
        assert_eq!(bytes.len(), 8 + 16 + 40);
        assert_eq!(
            u32::from_le_bytes(bytes[60..64].try_into().unwrap()),
            SOURCE_COVERED_BIT | LISTENER_COVERED_BIT
        );
    }
}
