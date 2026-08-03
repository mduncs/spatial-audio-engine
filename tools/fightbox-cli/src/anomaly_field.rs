//! Offline cheap proxy-field runner. It never constructs render effects or runs reflections.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fightbox_api::{Directivity, ExtentDescriptor, ReferenceLevel};
use fightbox_steam_audio::{
    AnomalyClass, AnomalyQuerySession, AnomalyRawSample, DirectOcclusionMode, GridSpec,
    MultiSourceDescriptor, ProxyCell, S3SimulationConfig, classify_grid,
    classify_sample_at_distance,
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
const MEGABLOCK_BLOCKS_PER_AXIS: usize = 6;
const MEGABLOCK_BLOCK_SIZE_M: f32 = 80.0;
const MEGABLOCK_STREET_WIDTH_M: f32 = 15.0;
const INNER_RADIUS_M: f32 = 4.0;
const OUTER_RADIUS_M: f32 = 10.0;
const FINE_INNER_SPACING_M: f32 = 0.1;
const COARSE_INNER_SPACING_M: f32 = 0.25;
const OUTER_SPACING_M: f32 = 1.0;
const DEFAULT_FINE_CORNER_COUNT: usize = 20;
const KNOWN_SPOT_ENU: [f32; 3] = [108.06, 303.91, 1.5];

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
        Some("corner-scan") => corner_scan(parse_corner_args(&arguments[1..])?),
        Some(other) => Err(CliError::new(format!(
            "unknown anomaly-field subcommand {other:?}; expected sweep or corner-scan"
        ))),
        None => Err(CliError::new(
            "anomaly-field requires the sweep or corner-scan subcommand",
        )),
    }
}

#[derive(Clone, Debug)]
struct CornerArgs {
    package: PathBuf,
    baked: PathBuf,
    fixture: PathBuf,
    source_id: String,
    source_height_m: Option<f32>,
    listener_height_m: f32,
    fine_corner_count: usize,
    output: PathBuf,
}

fn parse_corner_args(arguments: &[String]) -> Result<CornerArgs> {
    let mut package = None;
    let mut baked = None;
    let mut fixture = None;
    let mut source_id = None;
    let mut source_height_m = None;
    let mut listener_height_m = None;
    let mut fine_corner_count = None;
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
            "--fine-corner-count" => {
                let count = value.parse::<usize>().map_err(|_| {
                    CliError::new("--fine-corner-count requires a non-negative integer")
                })?;
                set_once(&mut fine_corner_count, count, flag)?;
            }
            "--output" => set_once(&mut output, PathBuf::from(value), flag)?,
            other => {
                return Err(CliError::new(format!(
                    "unknown anomaly-field corner-scan argument {other:?}"
                )));
            }
        }
    }
    Ok(CornerArgs {
        package: package.ok_or_else(|| CliError::new("missing required --package <path>"))?,
        baked: baked.ok_or_else(|| CliError::new("missing required --baked <path>"))?,
        fixture: fixture.ok_or_else(|| CliError::new("missing required --fixture <path>"))?,
        source_id: source_id.ok_or_else(|| CliError::new("missing required --source <id>"))?,
        source_height_m,
        listener_height_m: listener_height_m.unwrap_or(DEFAULT_LISTENER_HEIGHT_M),
        fine_corner_count: fine_corner_count.unwrap_or(DEFAULT_FINE_CORNER_COUNT),
        output: output.ok_or_else(|| CliError::new("missing required --output <path>"))?,
    })
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

#[derive(Clone, Copy, Debug, Serialize)]
struct CornerDefinition {
    id: usize,
    rank: usize,
    intersection_enu: [f32; 2],
    corner_enu: [f32; 2],
    corridor_relation: &'static str,
    clear_adjacent_segment_to_source: bool,
    distance_to_source_m: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CornerTier {
    Inner,
    Outer,
}

#[derive(Clone, Copy, Debug)]
struct ScannedDot {
    tier: CornerTier,
    row: usize,
    column: usize,
    scan_index: usize,
    spacing_m: f32,
    cell: ProxyCell,
}

#[derive(Clone, Serialize)]
struct CornerDotReport {
    corner_id: usize,
    corner_rank: usize,
    tier: CornerTier,
    row: usize,
    column: usize,
    scan_index: usize,
    spacing_m: f32,
    position_enu: [f32; 3],
    direct_audibility: Value,
    direct_loss_db: Value,
    path_sh_energy: Value,
    path_strength_db: Value,
    free_field_db: Value,
    score: Value,
    source_probe_covered: bool,
    listener_probe_covered: bool,
    classes: Vec<&'static str>,
}

#[derive(Clone, Serialize)]
struct GradientReport {
    delta_direct_audibility: f32,
    delta_direct_loss_db: Option<f32>,
    distance_m: f32,
    from_enu: [f32; 3],
    to_enu: [f32; 3],
}

#[derive(Serialize)]
struct CornerSummary {
    corner: CornerDefinition,
    inner_spacing_m: f32,
    inner_dot_count: usize,
    outer_dot_count: usize,
    anomaly_counts: BTreeMap<&'static str, usize>,
    sharpest_transition: Option<GradientReport>,
    inversion_slice_min_width_m: Option<f32>,
    inversion_slice_max_width_m: Option<f32>,
    worst_dot: Option<CornerDotReport>,
}

#[derive(Serialize)]
struct KnownSpotReport {
    requested_enu: [f32; 3],
    nearest_corner_id: usize,
    nearest_corner_enu: [f32; 2],
    nearest_dot: CornerDotReport,
    nearest_dot_distance_m: f32,
    reproduces_any_flag: bool,
    reproduces_inversion_signature: bool,
    inversion_slice_min_width_m: Option<f32>,
    inversion_slice_max_width_m: Option<f32>,
}

#[derive(Serialize)]
struct CornerScanManifest {
    schema_version: &'static str,
    package: String,
    baked: String,
    fixture: String,
    source_id: String,
    source_position_enu: [f32; 3],
    listener_height_m: f32,
    ranking_rule: &'static str,
    enumeration_rule: &'static str,
    scan_pattern: &'static str,
    inner_square_policy: &'static str,
    outer_overlap_policy: &'static str,
    fine_corner_count: usize,
    fine_corner_policy: &'static str,
    corner_count: usize,
    dot_count: usize,
    timings: TimingReport,
    anomaly_counts: BTreeMap<&'static str, usize>,
    computation_anomaly_count: usize,
    corners: Vec<CornerSummary>,
    top_hotspots: Vec<CornerDotReport>,
    known_spot: KnownSpotReport,
    samples_jsonl: &'static str,
    thresholds: Vec<ThresholdReport>,
}

fn enumerate_main_thoroughfare_corners(source: [f32; 2]) -> Vec<CornerDefinition> {
    let pitch = MEGABLOCK_BLOCK_SIZE_M + MEGABLOCK_STREET_WIDTH_M;
    let first_center = MEGABLOCK_STREET_WIDTH_M * 0.5;
    let half_street = MEGABLOCK_STREET_WIDTH_M * 0.5;
    let centers = (0..=MEGABLOCK_BLOCKS_PER_AXIS)
        .map(|index| first_center + index as f32 * pitch)
        .collect::<Vec<_>>();
    let source_axis = |value: f32, source_value: f32| (value - source_value).abs() < 0.01;
    let mut corners = Vec::new();
    for &east in &centers {
        for &north in &centers {
            let east_west = source_axis(north, source[1]);
            let north_south = source_axis(east, source[0]);
            if !east_west && !north_south {
                continue;
            }
            let relation = match (east_west, north_south) {
                (true, true) => "source_cross",
                (true, false) => "east_west_source_corridor",
                (false, true) => "north_south_source_corridor",
                (false, false) => unreachable!(),
            };
            for [dx, dy] in [
                [-half_street, -half_street],
                [half_street, -half_street],
                [-half_street, half_street],
                [half_street, half_street],
            ] {
                let distance_to_source_m =
                    ((east - source[0]).powi(2) + (north - source[1]).powi(2)).sqrt();
                corners.push(CornerDefinition {
                    id: 0,
                    rank: 0,
                    intersection_enu: [east, north],
                    corner_enu: [east + dx, north + dy],
                    corridor_relation: relation,
                    clear_adjacent_segment_to_source: true,
                    distance_to_source_m,
                });
            }
        }
    }
    corners.sort_by(|left, right| {
        right
            .clear_adjacent_segment_to_source
            .cmp(&left.clear_adjacent_segment_to_source)
            .then_with(|| {
                left.distance_to_source_m
                    .total_cmp(&right.distance_to_source_m)
            })
            .then_with(|| left.corner_enu[1].total_cmp(&right.corner_enu[1]))
            .then_with(|| left.corner_enu[0].total_cmp(&right.corner_enu[0]))
    });
    for (index, corner) in corners.iter_mut().enumerate() {
        corner.id = index + 1;
        corner.rank = index + 1;
    }
    corners
}

fn boustrophedon_indices(side: usize) -> Vec<usize> {
    let mut indices = Vec::with_capacity(side * side);
    for row in 0..side {
        if row % 2 == 0 {
            indices.extend((0..side).map(|column| row * side + column));
        } else {
            indices.extend((0..side).rev().map(|column| row * side + column));
        }
    }
    indices
}

fn classify_exact_sample(
    raw: AnomalyRawSample,
    source_spl_db: f32,
    source_position: fightbox_api::EnuVector3,
) -> ProxyCell {
    classify_sample_at_distance(
        raw,
        source_spl_db,
        distance(source_position, raw.position_enu),
    )
}

fn scan_square(
    query: &mut AnomalyQuerySession,
    fixture: &FixtureQuery,
    corner: CornerDefinition,
    listener_height_m: f32,
    radius_m: f32,
    spacing_m: f32,
    tier: CornerTier,
) -> Result<Vec<ScannedDot>> {
    let side = ((radius_m * 2.0 / spacing_m).round() as usize) + 1;
    let mut cells = vec![None; side * side];
    for (scan_index, row_major_index) in boustrophedon_indices(side).into_iter().enumerate() {
        let row = row_major_index / side;
        let column = row_major_index % side;
        let position = fightbox_steam_audio::EnuVector3::new(
            corner.corner_enu[0] - radius_m + column as f32 * spacing_m,
            corner.corner_enu[1] - radius_m + row as f32 * spacing_m,
            listener_height_m,
        );
        let raw = query.sample(position).map_err(|error| {
            CliError::new(format!(
                "corner {} query failed at {position:?}: {error}",
                corner.id
            ))
        })?;
        cells[row_major_index] = Some((
            scan_index,
            classify_exact_sample(raw, fixture.source_spl_db, fixture.source_position),
        ));
    }
    let mut row_major_cells = cells
        .iter()
        .map(|cell| cell.expect("all square cells queried").1)
        .collect::<Vec<_>>();
    classify_grid(&mut row_major_cells, side, side, spacing_m);
    let mut dots = cells
        .into_iter()
        .enumerate()
        .map(|(index, cell)| {
            let (scan_index, _) = cell.expect("all square cells queried");
            ScannedDot {
                tier,
                row: index / side,
                column: index % side,
                scan_index,
                spacing_m,
                cell: row_major_cells[index],
            }
        })
        .collect::<Vec<_>>();
    dots.sort_by_key(|dot| dot.scan_index);
    Ok(dots)
}

fn corner_scan(args: CornerArgs) -> Result<()> {
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
    let corners = enumerate_main_thoroughfare_corners([
        fixture.source_position.east_m,
        fixture.source_position.north_m,
    ]);
    let ranked_fine_corner_count = args.fine_corner_count.min(corners.len());
    let fine_corner_count = corners
        .iter()
        .enumerate()
        .filter(|(index, corner)| {
            *index < ranked_fine_corner_count || is_known_spot_corner(**corner)
        })
        .count();
    let query_started = Instant::now();
    let mut scans = Vec::with_capacity(corners.len());
    for (index, corner) in corners.iter().copied().enumerate() {
        let inner_spacing_m = if index < ranked_fine_corner_count || is_known_spot_corner(corner) {
            FINE_INNER_SPACING_M
        } else {
            COARSE_INNER_SPACING_M
        };
        let mut dots = scan_square(
            &mut query,
            &fixture,
            corner,
            args.listener_height_m,
            INNER_RADIUS_M,
            inner_spacing_m,
            CornerTier::Inner,
        )?;
        let outer = scan_square(
            &mut query,
            &fixture,
            corner,
            args.listener_height_m,
            OUTER_RADIUS_M,
            OUTER_SPACING_M,
            CornerTier::Outer,
        )?;
        dots.extend(outer.into_iter().filter(|dot| {
            let dx = (dot.cell.position_enu.x - corner.corner_enu[0]).abs();
            let dy = (dot.cell.position_enu.y - corner.corner_enu[1]).abs();
            dx > INNER_RADIUS_M + 0.001 || dy > INNER_RADIUS_M + 0.001
        }));
        scans.push((corner, dots));
        eprintln!(
            "fightbox: corner scan {}/{} rank={} inner_step={:.2}m dots={}",
            index + 1,
            corners.len(),
            corner.rank,
            inner_spacing_m,
            scans.last().map_or(0, |(_, dots)| dots.len())
        );
    }
    let query_seconds = query_started.elapsed().as_secs_f64();

    let directory = AtomicDir::create(output)?;
    let write_started = Instant::now();
    let mut jsonl = Vec::new();
    let mut summaries = Vec::with_capacity(scans.len());
    let mut total_counts = empty_class_counts();
    let mut total_dots = 0;
    let mut hotspot_candidates = Vec::new();
    for (corner, dots) in &scans {
        total_dots += dots.len();
        for dot in dots {
            for class in AnomalyClass::ALL {
                if dot.cell.flags.contains(class) {
                    *total_counts.get_mut(class.id()).expect("class key exists") += 1;
                }
            }
            serde_json::to_writer(&mut jsonl, &corner_dot_report(*corner, *dot))
                .map_err(|error| CliError::new(format!("cannot serialize corner dot: {error}")))?;
            jsonl.push(b'\n');
        }
        let inner = dots
            .iter()
            .copied()
            .filter(|dot| dot.tier == CornerTier::Inner)
            .collect::<Vec<_>>();
        let outer_count = dots.len() - inner.len();
        let inner_spacing_m = inner
            .first()
            .map_or(COARSE_INNER_SPACING_M, |dot| dot.spacing_m);
        let anomaly_counts = AnomalyClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class.id(),
                    dots.iter()
                        .filter(|dot| dot.cell.flags.contains(class))
                        .count(),
                )
            })
            .collect();
        let widths = inversion_slice_widths(&inner, inner_spacing_m);
        let worst = dots
            .iter()
            .copied()
            .filter(|dot| !dot.cell.flags.is_empty())
            .max_by(|left, right| severity(left.cell).total_cmp(&severity(right.cell)));
        if let Some(worst) = worst {
            hotspot_candidates.push((*corner, worst));
        }
        summaries.push(CornerSummary {
            corner: *corner,
            inner_spacing_m,
            inner_dot_count: inner.len(),
            outer_dot_count: outer_count,
            anomaly_counts,
            sharpest_transition: sharpest_transition(&inner),
            inversion_slice_min_width_m: widths.map(|value| value.0),
            inversion_slice_max_width_m: widths.map(|value| value.1),
            worst_dot: worst.map(|dot| corner_dot_report(*corner, dot)),
        });
    }
    hotspot_candidates.sort_by(|left, right| {
        severity(right.1.cell)
            .total_cmp(&severity(left.1.cell))
            .then_with(|| left.0.rank.cmp(&right.0.rank))
    });
    let top_hotspots = hotspot_candidates
        .into_iter()
        .take(10)
        .map(|(corner, dot)| corner_dot_report(corner, dot))
        .collect();
    let known_spot = known_spot_report(&scans, &summaries)?;
    write_bytes_atomic(&directory.temp_path().join("samples.jsonl"), &jsonl)?;
    let computation_anomaly_count = scans
        .iter()
        .flat_map(|(_, dots)| dots)
        .filter(|dot| has_computation_anomaly(dot.cell))
        .count();
    let mut manifest = CornerScanManifest {
        schema_version: "fightbox.anomaly-corner-scan.v1",
        package: args.package.display().to_string(),
        baked: args.baked.display().to_string(),
        fixture: args.fixture.display().to_string(),
        source_id: args.source_id,
        source_position_enu: [
            fixture.source_position.east_m,
            fixture.source_position.north_m,
            fixture.source_position.up_m,
        ],
        listener_height_m: args.listener_height_m,
        ranking_rule: "clear source-aligned adjacent street segment first, then planar distance from intersection to source, then stable ENU tie-break",
        enumeration_rule: "6x6 fixture city: 80m blocks + 15m streets; four street-edge corners at each of 13 intersections on the source-aligned east-west/north-south thoroughfares",
        scan_pattern: "boustrophedon squares: inner +/-4m; outer +/-10m",
        inner_square_policy: "all 81x81 or 33x33 inner-square dots retained, including dots outside the nominal 4m circle",
        outer_overlap_policy: "1m outer-square dots inside the +/-4m inner square queried for neighbour classification but omitted from JSONL as duplicates",
        fine_corner_count,
        fine_corner_policy: "top-ranked requested count plus the known-spot [110,300] corner are scanned at 0.1m",
        corner_count: corners.len(),
        dot_count: total_dots,
        timings: TimingReport {
            load_and_session_seconds,
            query_seconds,
            write_seconds: 0.0,
            total_seconds: 0.0,
        },
        anomaly_counts: total_counts,
        computation_anomaly_count,
        corners: summaries,
        top_hotspots,
        known_spot,
        samples_jsonl: "samples.jsonl",
        thresholds: AnomalyClass::ALL
            .into_iter()
            .map(|class| ThresholdReport {
                id: class.id(),
                threshold: class.threshold(),
                rationale: class.rationale(),
            })
            .collect(),
    };
    manifest.timings.write_seconds = write_started.elapsed().as_secs_f64();
    manifest.timings.total_seconds = total_started.elapsed().as_secs_f64();
    write_json_atomic(&directory.temp_path().join("manifest.json"), &manifest)?;
    directory.commit()?;
    println!(
        "{}",
        serde_json::to_string(&manifest)
            .map_err(|error| CliError::new(format!("cannot serialize corner summary: {error}")))?
    );
    Ok(())
}

fn empty_class_counts() -> BTreeMap<&'static str, usize> {
    AnomalyClass::ALL
        .into_iter()
        .map(|class| (class.id(), 0))
        .collect()
}

fn is_known_spot_corner(corner: CornerDefinition) -> bool {
    (corner.corner_enu[0] - 110.0).abs() < 0.01 && (corner.corner_enu[1] - 300.0).abs() < 0.01
}

fn json_float(value: f32) -> Value {
    if value.is_finite() {
        Value::from(value)
    } else {
        Value::String(if value.is_nan() {
            "NaN".to_owned()
        } else if value.is_sign_positive() {
            "+inf".to_owned()
        } else {
            "-inf".to_owned()
        })
    }
}

fn corner_dot_report(corner: CornerDefinition, dot: ScannedDot) -> CornerDotReport {
    CornerDotReport {
        corner_id: corner.id,
        corner_rank: corner.rank,
        tier: dot.tier,
        row: dot.row,
        column: dot.column,
        scan_index: dot.scan_index,
        spacing_m: dot.spacing_m,
        position_enu: [
            dot.cell.position_enu.x,
            dot.cell.position_enu.y,
            dot.cell.position_enu.z,
        ],
        direct_audibility: json_float(dot.cell.direct_audibility),
        direct_loss_db: json_float(dot.cell.direct_loss_db),
        path_sh_energy: json_float(dot.cell.path_sh_energy),
        path_strength_db: json_float(dot.cell.path_strength_db),
        free_field_db: json_float(dot.cell.free_field_db),
        score: json_float(dot.cell.score),
        source_probe_covered: dot.cell.source_probe_covered,
        listener_probe_covered: dot.cell.listener_probe_covered,
        classes: AnomalyClass::ALL
            .into_iter()
            .filter(|class| dot.cell.flags.contains(*class))
            .map(AnomalyClass::id)
            .collect(),
    }
}

fn sharpest_transition(inner: &[ScannedDot]) -> Option<GradientReport> {
    let side = (inner.len() as f64).sqrt() as usize;
    let by_cell = inner
        .iter()
        .map(|dot| ((dot.row, dot.column), *dot))
        .collect::<BTreeMap<_, _>>();
    let mut best: Option<GradientReport> = None;
    for row in 0..side {
        for column in 0..side {
            let from = by_cell.get(&(row, column))?;
            for neighbour in [(row, column + 1), (row + 1, column)] {
                let Some(to) = by_cell.get(&neighbour) else {
                    continue;
                };
                if !from.cell.direct_audibility.is_finite()
                    || !to.cell.direct_audibility.is_finite()
                {
                    continue;
                }
                let delta = (from.cell.direct_audibility - to.cell.direct_audibility).abs();
                if best
                    .as_ref()
                    .is_none_or(|current| delta > current.delta_direct_audibility)
                {
                    let delta_db = (from.cell.direct_loss_db - to.cell.direct_loss_db).abs();
                    best = Some(GradientReport {
                        delta_direct_audibility: delta,
                        delta_direct_loss_db: delta_db.is_finite().then_some(delta_db),
                        distance_m: from.spacing_m,
                        from_enu: [
                            from.cell.position_enu.x,
                            from.cell.position_enu.y,
                            from.cell.position_enu.z,
                        ],
                        to_enu: [
                            to.cell.position_enu.x,
                            to.cell.position_enu.y,
                            to.cell.position_enu.z,
                        ],
                    });
                }
            }
        }
    }
    best
}

fn inversion_slice_widths(inner: &[ScannedDot], spacing_m: f32) -> Option<(f32, f32)> {
    let side = (inner.len() as f64).sqrt() as usize;
    let flagged = inner
        .iter()
        .map(|dot| {
            (
                (dot.row, dot.column),
                dot.cell.flags.contains(AnomalyClass::InversionSignature),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut widths = Vec::new();
    for fixed in 0..side {
        for transpose in [false, true] {
            let mut run = 0;
            for moving in 0..side {
                let key = if transpose {
                    (moving, fixed)
                } else {
                    (fixed, moving)
                };
                if flagged.get(&key).copied().unwrap_or(false) {
                    run += 1;
                } else if run > 0 {
                    widths.push(run as f32 * spacing_m);
                    run = 0;
                }
            }
            if run > 0 {
                widths.push(run as f32 * spacing_m);
            }
        }
    }
    widths.into_iter().fold(None, |range, width| {
        Some(match range {
            None => (width, width),
            Some((minimum, maximum)) => (minimum.min(width), maximum.max(width)),
        })
    })
}

fn has_computation_anomaly(cell: ProxyCell) -> bool {
    AnomalyClass::ALL
        .into_iter()
        .filter(|class| *class != AnomalyClass::InversionSignature)
        .any(|class| cell.flags.contains(class))
}

fn known_spot_report(
    scans: &[(CornerDefinition, Vec<ScannedDot>)],
    summaries: &[CornerSummary],
) -> Result<KnownSpotReport> {
    let (corner, dots) = scans
        .iter()
        .min_by(|(left, _), (right, _)| {
            let left_distance = (left.corner_enu[0] - KNOWN_SPOT_ENU[0]).powi(2)
                + (left.corner_enu[1] - KNOWN_SPOT_ENU[1]).powi(2);
            let right_distance = (right.corner_enu[0] - KNOWN_SPOT_ENU[0]).powi(2)
                + (right.corner_enu[1] - KNOWN_SPOT_ENU[1]).powi(2);
            left_distance.total_cmp(&right_distance)
        })
        .ok_or_else(|| CliError::new("corner scan enumerated no corners"))?;
    let dot = dots
        .iter()
        .copied()
        .filter(|dot| dot.tier == CornerTier::Inner)
        .min_by(|left, right| {
            let distance = |dot: &ScannedDot| {
                (dot.cell.position_enu.x - KNOWN_SPOT_ENU[0]).powi(2)
                    + (dot.cell.position_enu.y - KNOWN_SPOT_ENU[1]).powi(2)
            };
            distance(left).total_cmp(&distance(right))
        })
        .ok_or_else(|| CliError::new("known-spot corner has no inner dots"))?;
    let summary = summaries
        .iter()
        .find(|summary| summary.corner.id == corner.id)
        .expect("every scan has a summary");
    let dx = dot.cell.position_enu.x - KNOWN_SPOT_ENU[0];
    let dy = dot.cell.position_enu.y - KNOWN_SPOT_ENU[1];
    Ok(KnownSpotReport {
        requested_enu: KNOWN_SPOT_ENU,
        nearest_corner_id: corner.id,
        nearest_corner_enu: corner.corner_enu,
        nearest_dot: corner_dot_report(*corner, dot),
        nearest_dot_distance_m: (dx * dx + dy * dy).sqrt(),
        reproduces_any_flag: !dot.cell.flags.is_empty(),
        reproduces_inversion_signature: dot.cell.flags.contains(AnomalyClass::InversionSignature),
        inversion_slice_min_width_m: summary.inversion_slice_min_width_m,
        inversion_slice_max_width_m: summary.inversion_slice_max_width_m,
    })
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

    fn clean_raw(position: fightbox_steam_audio::EnuVector3) -> AnomalyRawSample {
        AnomalyRawSample {
            position_enu: position,
            direct_audibility: 1.0,
            path_eq: [0.1; 3],
            path_sh_energy: 0.1,
            path_coefficient_min: 0.1,
            path_coefficient_max: 0.1,
            source_probe_covered: true,
            listener_probe_covered: true,
            direct_path_energy: None,
            reflection_energy: None,
        }
    }

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

    #[test]
    fn megablock_main_thoroughfare_corner_enumeration_is_stable() {
        let corners = enumerate_main_thoroughfare_corners([292.5, 292.5]);
        assert_eq!(corners.len(), 52);
        assert!(corners.iter().any(|corner| {
            corner.intersection_enu == [102.5, 292.5] && corner.corner_enu == [110.0, 300.0]
        }));
        assert!(corners.iter().any(|corner| {
            corner.intersection_enu == [292.5, 482.5] && corner.corner_enu == [285.0, 475.0]
        }));
        assert_eq!(corners[0].corridor_relation, "source_cross");
    }

    #[test]
    fn boustrophedon_order_reverses_every_other_row() {
        assert_eq!(boustrophedon_indices(3), vec![0, 1, 2, 5, 4, 3, 6, 7, 8]);
    }

    #[test]
    fn exact_sample_path_passes_all_local_classifier_classes_through() {
        let position = fightbox_steam_audio::EnuVector3::new(10.0, 10.0, 1.5);
        let source = fightbox_api::EnuVector3::new(0.0, 0.0, 1.5);
        let mut raw = clean_raw(position);
        raw.direct_audibility = 0.01;
        raw.path_eq = [0.01; 3];
        raw.path_sh_energy = 0.01;
        let cell = classify_exact_sample(raw, 105.0, source);
        assert!(cell.flags.contains(AnomalyClass::InversionSignature));

        let mut raw = clean_raw(position);
        raw.path_sh_energy = f32::NAN;
        let cell = classify_exact_sample(raw, 105.0, source);
        assert!(cell.flags.contains(AnomalyClass::InvalidEnergy));

        let mut raw = clean_raw(position);
        raw.direct_audibility = f32::NAN;
        let cell = classify_exact_sample(raw, 105.0, source);
        assert!(cell.flags.contains(AnomalyClass::InvalidCoefficient));

        let mut raw = clean_raw(position);
        raw.path_sh_energy = 0.0;
        let cell = classify_exact_sample(raw, 105.0, source);
        assert!(cell.flags.contains(AnomalyClass::ZeroPathWithCoverage));
    }
}
