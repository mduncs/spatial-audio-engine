//! One-command OSM street scenes; package/config-bound bake reuse.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use fightbox_evidence::sha256_hex;
use fightbox_steam_audio::{ProbeMask, STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION};
use fightbox_world::MaterialTable;
use serde_json::{Value, json};

use crate::atomicio::{AtomicDir, validate_output_path, write_bytes_atomic, write_json_atomic};
use crate::bake_memory::{BakeGuard, MemoryBudget};
use crate::city::{self, BakeConfig};
use crate::city_place::{self, Projection};
use crate::error::{CliError, Result};

mod assessor;
mod corridor;
mod detail;
mod graded;
mod lidar;
mod parts;

const MAX_PROBES: u64 = 26_000;
// Whole footprints crossing the query edge are retained, with the existing
// ground margin, so the acoustic mesh can be larger than the requested square.
const MAX_AREA_M2: f64 = 400_000.0;
const MAX_QUERY_AREA_M2: f64 = 250_001.0;
const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const NOMINATIM_URL: &str = "https://nominatim.openstreetmap.org/search";
const BUILD_SCHEMA: &str = "fightbox.city-build.v1";
const PROBES_SCHEMA: &str = "fightbox.city-probes.v1";

struct BuildOptions {
    osm: Option<PathBuf>,
    bbox: Option<[f64; 4]>,
    place: Option<String>,
    center: Option<[f64; 2]>,
    radius_m: f64,
    output: PathBuf,
    bake: BakeConfig,
    force_rebake: bool,
    allow_large_bake: bool,
    /// GiB a bake may use; `None` caps it at half of host RAM.
    max_bake_memory_gib: Option<f64>,
    heights: HeightSource,
    /// Legacy uniform floor: keep probes generated inside buildings and on
    /// low roofs (the bake key then matches pre-mask builds).
    keep_interior_probes: bool,
    corridor: Option<PathBuf>,
    corridor_width_m: f32,
    /// Full probe density only where the listener walks; `--corridor` routes
    /// then mark walked streets instead of bounding the bake.
    graded: Option<graded::GradedOptions>,
    /// City detail beyond footprints: LiDAR alley fences, assessor facade,
    /// roof and garage materials, and rail embankments and decks.
    fences: bool,
    assessor_materials: bool,
    assessor_required: bool,
    rail: bool,
}

/// `auto` measures heights from LiDAR where a source covers a fetched area
/// (Cook County today) and otherwise keeps OSM height/levels; `--osm` input
/// stays offline.
#[derive(Clone, Copy, Debug, PartialEq)]
enum HeightSource {
    Auto,
    Lidar,
    Osm,
}

fn parse(args: &[String]) -> Result<BuildOptions> {
    let mut osm = None;
    let mut bbox = None;
    let mut place = None;
    let mut center = None;
    let mut radius_m = None;
    let mut output = None;
    let mut force_rebake = false;
    let mut allow_large_bake = false;
    let mut heights = None;
    let mut keep_interior_probes = false;
    let mut fences = false;
    let mut assessor_materials = None;
    let mut rail = false;
    let mut corridor = None;
    let mut corridor_width_m = None;
    let mut max_bake_memory_gib = None;
    let mut graded = false;
    let mut fine_radius_m = None;
    let mut coarse_spacing_m = None;
    let mut bake_args = vec![
        "--package".into(),
        "unused".into(),
        "--output".into(),
        "unused".into(),
    ];
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--force-rebake" => set_flag(&mut force_rebake, flag)?,
            "--allow-large-bake" => set_flag(&mut allow_large_bake, flag)?,
            "--keep-interior-probes" => set_flag(&mut keep_interior_probes, flag)?,
            "--fences" => set_flag(&mut fences, flag)?,
            "--rail" => set_flag(&mut rail, flag)?,
            "--graded" => set_flag(&mut graded, flag)?,
            "--osm" | "--bbox" | "--place" | "--center" | "--radius-m" | "--output"
            | "--heights" | "--corridor" | "--corridor-width-m" | "--materials"
            | "--max-bake-memory-gb" | "--fine-radius-m" | "--coarse-spacing-m" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CliError::new(format!("{flag} requires a value")))?;
                match flag.as_str() {
                    "--osm" => crate::set_once(&mut osm, PathBuf::from(value), flag)?,
                    "--output" => crate::set_once(&mut output, PathBuf::from(value), flag)?,
                    "--corridor" => crate::set_once(&mut corridor, PathBuf::from(value), flag)?,
                    "--materials" => {
                        let assessor = match value.as_str() {
                            "assessor" => true,
                            "default" => false,
                            _ => return Err(CliError::new("--materials takes assessor or default")),
                        };
                        crate::set_once(&mut assessor_materials, assessor, flag)?;
                    }
                    "--max-bake-memory-gb" => {
                        let gib = value
                            .parse::<f64>()
                            .ok()
                            .filter(|gib| gib.is_finite() && *gib > 0.0)
                            .ok_or_else(|| {
                                CliError::new("--max-bake-memory-gb needs a positive GiB count")
                            })?;
                        crate::set_once(&mut max_bake_memory_gib, gib, flag)?;
                    }
                    "--corridor-width-m" => {
                        let width = value
                            .parse::<f32>()
                            .ok()
                            .filter(|width| width.is_finite() && *width > 0.0)
                            .ok_or_else(|| {
                                CliError::new("--corridor-width-m needs a finite positive number")
                            })?;
                        crate::set_once(&mut corridor_width_m, width, flag)?;
                    }
                    "--fine-radius-m" | "--coarse-spacing-m" => {
                        let metres = value
                            .parse::<f32>()
                            .ok()
                            .filter(|metres| metres.is_finite() && *metres > 0.0)
                            .ok_or_else(|| {
                                CliError::new(format!("{flag} needs a finite positive number"))
                            })?;
                        if flag == "--fine-radius-m" {
                            crate::set_once(&mut fine_radius_m, metres, flag)?;
                        } else {
                            crate::set_once(&mut coarse_spacing_m, metres, flag)?;
                        }
                    }
                    "--heights" => {
                        let source = match value.as_str() {
                            "auto" => HeightSource::Auto,
                            "lidar" => HeightSource::Lidar,
                            "osm" => HeightSource::Osm,
                            _ => return Err(CliError::new("--heights takes auto, lidar or osm")),
                        };
                        crate::set_once(&mut heights, source, flag)?;
                    }
                    "--place" => {
                        if value.trim().is_empty() {
                            return Err(CliError::new("--place needs a place name"));
                        }
                        crate::set_once(&mut place, value.trim().to_owned(), flag)?;
                    }
                    "--center" => {
                        crate::set_once(&mut center, city_place::parse_center(value)?, flag)?
                    }
                    "--radius-m" => {
                        let radius = value
                            .parse::<f64>()
                            .ok()
                            .filter(|r| r.is_finite() && *r > 0.0)
                            .ok_or_else(|| {
                                CliError::new("--radius-m needs a finite positive number")
                            })?;
                        crate::set_once(&mut radius_m, radius, flag)?;
                    }
                    _ => crate::set_once(&mut bbox, parse_bbox(value)?, flag)?,
                }
            }
            "--package" => {
                return Err(CliError::new(
                    "city build takes --place, --center, --osm or --bbox, not --package",
                ));
            }
            _ => {
                bake_args.push(flag.clone());
                bake_args.push(
                    iter.next()
                        .ok_or_else(|| CliError::new(format!("{flag} requires a value")))?
                        .clone(),
                );
            }
        }
    }
    if [
        osm.is_some(),
        bbox.is_some(),
        place.is_some(),
        center.is_some(),
    ]
    .into_iter()
    .filter(|v| *v)
    .count()
        != 1
    {
        return Err(CliError::new(
            "city build requires exactly one of --place <text>, --center lat,lon, --osm <path> or --bbox south,west,north,east",
        ));
    }
    if radius_m.is_some() && place.is_none() && center.is_none() {
        return Err(CliError::new("--radius-m requires --place or --center"));
    }
    if corridor_width_m.is_some() && corridor.is_none() {
        return Err(CliError::new("--corridor-width-m requires --corridor"));
    }
    if (fine_radius_m.is_some() || coarse_spacing_m.is_some()) && !graded {
        return Err(CliError::new(
            "--fine-radius-m and --coarse-spacing-m require --graded",
        ));
    }
    let heights = heights.unwrap_or(HeightSource::Auto);
    if fences && heights == HeightSource::Osm {
        return Err(CliError::new("--fences reads LiDAR; add --heights lidar or auto"));
    }
    for (flag, default) in [("--path-range-m", "1500"), ("--visibility-range-m", "40")] {
        if !bake_args.iter().any(|argument| argument == flag) {
            bake_args.extend([flag.into(), default.into()]);
        }
    }
    if (place.is_some() || center.is_some()) && !bake_args.iter().any(|arg| arg == "--bake-threads")
    {
        bake_args.extend(["--bake-threads".into(), "4".into()]);
    }
    let (_, _, bake) = crate::parse_city_bake_args(&bake_args)?;
    // A local --osm file remains a portable, offline input. Geographic builds
    // request the available real-world detail by default.
    let geographic = osm.is_none();
    let assessor_required = assessor_materials == Some(true);
    let graded = graded
        .then(|| {
            let defaults = graded::GradedOptions::DEFAULT;
            let options = graded::GradedOptions {
                fine_radius_m: fine_radius_m.unwrap_or(defaults.fine_radius_m),
                coarse_spacing_m: coarse_spacing_m.unwrap_or(defaults.coarse_spacing_m),
            };
            options.stride(bake.probe_spacing_m).map(|_| options)
        })
        .transpose()?;
    Ok(BuildOptions {
        osm,
        bbox,
        place,
        center,
        radius_m: radius_m.unwrap_or(250.0),
        bake,
        force_rebake,
        allow_large_bake,
        max_bake_memory_gib,
        heights,
        keep_interior_probes,
        fences: fences || (geographic && heights != HeightSource::Osm),
        assessor_materials: assessor_materials.unwrap_or(geographic),
        assessor_required,
        rail: rail || geographic,
        corridor,
        corridor_width_m: corridor_width_m.unwrap_or(20.0),
        graded,
        output: output.ok_or_else(|| CliError::new("missing required --output <directory>"))?,
    })
}

fn set_flag(value: &mut bool, flag: &str) -> Result<()> {
    if *value {
        return Err(CliError::new(format!("duplicate argument {flag}")));
    }
    *value = true;
    Ok(())
}

fn parse_bbox(value: &str) -> Result<[f64; 4]> {
    let values = value
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| CliError::new("--bbox requires south,west,north,east in degrees"))?;
    let bbox: [f64; 4] = values
        .try_into()
        .map_err(|_| CliError::new("--bbox requires four coordinates"))?;
    let [south, west, north, east] = bbox;
    if bbox.iter().any(|value| !value.is_finite())
        || !(-90.0..90.0).contains(&south)
        || !(-90.0..90.0).contains(&north)
        || !(-180.0..=180.0).contains(&west)
        || !(-180.0..=180.0).contains(&east)
        || south >= north
        || west >= east
    {
        return Err(CliError::new(
            "--bbox needs increasing, finite latitude/longitude bounds; split areas crossing the antimeridian",
        ));
    }
    Ok(bbox)
}

fn bbox_area([south, west, north, east]: [f64; 4]) -> f64 {
    let radius = 6_371_008.8;
    (north - south).to_radians()
        * radius
        * (east - west).to_radians()
        * radius
        * ((north + south) * 0.5).to_radians().cos()
}

fn overpass_request([south, west, north, east]: [f64; 4], rail: bool) -> (&'static str, String) {
    let bounds = format!("{south},{west},{north},{east}");
    // Rail ways join the query only on request, so plain builds keep their
    // cached Overpass response.
    let rail = if rail {
        format!(
            "  way[\"railway\"~\"^(rail|subway|light_rail)$\"]({bounds});\n  way[\"barrier\"=\"retaining_wall\"]({bounds});\n"
        )
    } else {
        String::new()
    };
    (
        OVERPASS_URL,
        format!(
            "[out:json][timeout:90];\n(\n  way[\"building\"]({bounds});\n  relation[\"building\"]({bounds});\n  way[\"highway\"]({bounds});\n{rail});\nout body;\n>;\nout skel qt;\n"
        ),
    )
}

fn guard_size(probes: u64, area_m2: f64, allow_large: bool) -> Result<()> {
    if !allow_large && (probes > MAX_PROBES || area_m2 > MAX_AREA_M2) {
        return Err(CliError::new(format!(
            "large bake refused: at most {probes} probes, {area_m2:.0} m² (limits {MAX_PROBES} probes / {MAX_AREA_M2:.0} m²); use --allow-large-bake after reviewing the cost"
        )));
    }
    Ok(())
}

fn guard_query_area(area_m2: f64, allow_large: bool) -> Result<()> {
    if !allow_large && area_m2 > MAX_QUERY_AREA_M2 {
        return Err(CliError::new(
            "area exceeds the 250 m radius neighborhood default; use --allow-large-bake after reviewing the cost",
        ));
    }
    Ok(())
}

fn bake_planning_seconds(probes: u64, config: &BakeConfig) -> f64 {
    // Loop, background priority, 4 threads: 12,540 probes / 97.24 s and
    // 20,280 / 389.93 s. A conservative cubic fit covers both full bake stages.
    97.25 * (probes as f64 / 12_540.0).powi(3) * (4.0 / f64::from(config.bake_threads))
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    let options = parse(args)?;
    if let Some(bbox) = options.bbox {
        guard_query_area(bbox_area(bbox), options.allow_large_bake)?;
    }
    if options.place.is_some() || options.center.is_some() {
        guard_query_area(4.0 * options.radius_m.powi(2), options.allow_large_bake)?;
    }
    let started = Instant::now();
    let output = validate_output_path(&options.output)?;
    let marker = output.join("city-build.json");
    if output.exists()
        && output
            .read_dir()
            .map_err(|error| CliError::new(error.to_string()))?
            .next()
            .is_some()
    {
        let previous = read_json(&marker).ok();
        if previous
            .as_ref()
            .and_then(|value| value["schema_version"].as_str())
            != Some(BUILD_SCHEMA)
        {
            return Err(CliError::new(
                "city build output is nonempty and is not a generated city build directory",
            ));
        }
    }
    std::fs::create_dir_all(&output).map_err(|error| CliError::new(error.to_string()))?;
    let lock_path = output.join(".city-build.lock");
    let _lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|error| {
            CliError::new(format!(
                "cannot claim city build directory (another build or interrupted run): {error}"
            ))
        })?;
    let _lock = BuildLock(lock_path.clone());
    write_json_atomic(&marker, &json!({"schema_version": BUILD_SCHEMA}))?;
    let mut stages = Vec::new();

    let stage = Instant::now();
    let mut center = options.center;
    let mut geocoding = Value::Null;
    let mut street_lookup = Value::Null;
    let mut display_name = options.place.clone().unwrap_or_else(|| "City".into());
    if let Some(place) = &options.place {
        let intersection = city_place::intersection(place);
        let search = intersection
            .as_ref()
            .map_or(place.as_str(), |intersection| intersection.locality);
        let url = std::env::var("FIGHTBOX_NOMINATIM_URL").unwrap_or_else(|_| NOMINATIM_URL.into());
        let response = city_place::fetch_cached(
            &output,
            "nominatim",
            &url,
            &[("q", search), ("format", "jsonv2"), ("limit", "1")],
        )?;
        let (location, name) = city_place::parse_nominatim(&response.raw)?;
        center = Some(location);
        if let Some(intersection) = intersection {
            let query = city_place::intersection_query(&response.raw, &intersection)?;
            let url =
                std::env::var("FIGHTBOX_OVERPASS_URL").unwrap_or_else(|_| OVERPASS_URL.into());
            let streets =
                city_place::fetch_cached(&output, "street_lookup", &url, &[("data", &query)])?;
            center = Some(city_place::resolve_intersection(
                &streets.raw,
                &intersection,
                location,
            )?);
            street_lookup = streets.metadata;
        }
        display_name = name;
        geocoding = response.metadata;
        record_stage(&mut stages, "geocode", stage, response.cache_hit);
    }
    let requested_bbox = match center {
        Some(center) => Some(city_place::bbox_from_center(center, options.radius_m)?),
        None => options.bbox,
    };
    if let Some(bbox) = requested_bbox {
        guard_query_area(bbox_area(bbox), options.allow_large_bake)?;
    }
    let stage = Instant::now();
    let mut query_text = Value::Null;
    let mut input_metadata = Value::Null;
    let mut input_cache_hit = false;
    let raw = if let Some(path) = &options.osm {
        let raw = std::fs::read(path)
            .map_err(|error| CliError::new(format!("cannot read {}: {error}", path.display())))?;
        write_bytes_atomic(&output.join("overpass_raw.json"), &raw)?;
        raw
    } else {
        let (default_url, query) =
            overpass_request(requested_bbox.expect("parser requires an input"), options.rail);
        let url = std::env::var("FIGHTBOX_OVERPASS_URL").unwrap_or_else(|_| default_url.into());
        let response = city_place::fetch_cached(&output, "overpass", &url, &[("data", &query)])?;
        query_text = json!(query);
        input_metadata = response.metadata;
        input_cache_hit = response.cache_hit;
        response.raw
    };
    record_stage(&mut stages, "input", stage, input_cache_hit);

    let stage = Instant::now();
    let parts_response = if options.osm.is_none() {
        let query = parts::request(requested_bbox.expect("geographic input"));
        let url = std::env::var("FIGHTBOX_OVERPASS_URL").unwrap_or_else(|_| OVERPASS_URL.into());
        Some(city_place::fetch_cached(
            &output,
            "overpass_parts",
            &url,
            &[("data", &query)],
        )?)
    } else {
        // Recorded local inputs may include a companion parts response. Never
        // fetch on the portable --osm route.
        let companion = options
            .osm
            .as_ref()
            .expect("local input")
            .with_file_name("overpass_parts_raw.json");
        if companion.exists() {
            let raw =
                std::fs::read(&companion).map_err(|error| CliError::new(error.to_string()))?;
            let metadata_path = companion.with_file_name("overpass_parts_request.json");
            let metadata = if metadata_path.exists() {
                let metadata = read_json(&metadata_path)?;
                if metadata["response_sha256"] != sha256_hex(&raw) {
                    return Err(CliError::new(
                        "local building parts response hash does not match its request cache",
                    ));
                }
                metadata
            } else {
                json!({"request": {"url": format!("file://{}", companion.display()), "parameters": []},
                    "response_sha256": sha256_hex(&raw), "fetched_at_unix_s": Value::Null, "data_timestamp": Value::Null})
            };
            write_bytes_atomic(&output.join("overpass_parts_raw.json"), &raw)?;
            write_json_atomic(&output.join("overpass_parts_request.json"), &metadata)?;
            Some(city_place::Response {
                metadata,
                raw,
                cache_hit: true,
            })
        } else {
            None
        }
    };
    let building_parts = parts_response
        .as_ref()
        .map(|response| parts::parse(&response.raw))
        .transpose()?
        .unwrap_or_default();
    record_stage(
        &mut stages,
        "parts",
        stage,
        parts_response
            .as_ref()
            .is_some_and(|response| response.cache_hit),
    );

    let stage = Instant::now();
    let mut geojson = convert_osm(&raw)?;
    let lidar = measured_heights(&options, &output, &geojson, requested_bbox)?;
    record_stage(
        &mut stages,
        "heights",
        stage,
        lidar.as_ref().is_some_and(|measured| measured.cache_hit),
    );

    let stage = Instant::now();
    if let Some(measured) = &lidar {
        apply_height_overrides(&mut geojson, &measured.heights)?;
    }
    let parts_summary = parts::apply(&mut geojson, &building_parts)?;
    let bounds = requested_bbox.unwrap_or(geojson_bounds(&geojson)?);
    let projection = Projection {
        center: center.unwrap_or([(bounds[0] + bounds[2]) * 0.5, (bounds[1] + bounds[3]) * 0.5]),
    };
    let streets = osm_streets(&raw, projection, bounds)?;
    let walked = options
        .corridor
        .as_deref()
        .map(|path| corridor::load(path, options.corridor_width_m, projection))
        .transpose()?;
    let probe_mask = match options.graded {
        Some(graded_options) => ProbeMask {
            drop_over_solids: !options.keep_interior_probes,
            corridor: None,
            graded: Some(graded::density(
                graded_options,
                graded_options.stride(options.bake.probe_spacing_m)?,
                walked,
                &graded::street_corners(&raw, projection, bounds)?,
            )),
        },
        None => ProbeMask {
            drop_over_solids: !options.keep_interior_probes,
            corridor: walked,
            graded: None,
        },
    };
    let mut city_detail = serde_json::Map::new();
    let assessor_available = options.assessor_materials && lidar::covers(geojson_bounds(&geojson)?);
    if options.assessor_required && !assessor_available {
        return Err(CliError::new(
            "--materials assessor covers Cook County, Illinois only",
        ));
    }
    if assessor_available {
        let footprints = geojson_bounds(&geojson)?;
        let stage = Instant::now();
        let assessed = assessor::apply(&output, &mut geojson, footprints)?;
        eprintln!(
            "fightbox: assessor materials {} ({} residential parcels); {}",
            assessed.metadata["materials"],
            assessed.metadata["residential_parcels"],
            assessor::ATTRIBUTION
        );
        record_stage(&mut stages, "assessor", stage, assessed.cache_hit);
        city_detail.insert("materials".into(), assessed.metadata);
    }
    let mut heights = height_coverage(&geojson, lidar.as_ref().map(|measured| &measured.heights));
    heights["parts"] = parts_summary;
    let mut detail_features = Vec::new();
    if options.fences {
        match &lidar {
            Some(measured) => {
                let footprints = detail::Footprints::from_geojson(&geojson, projection);
                let fences = detail::fences(&streets, &footprints, &measured.rasters, projection);
                let alleys = streets.iter().filter(|street| street.service == "alley").count();
                eprintln!(
                    "fightbox: {} LiDAR fence run(s) along {alleys} alley segment(s)",
                    fences.len()
                );
                city_detail.insert(
                    "fences".into(),
                    json!({"count": fences.len(), "alley_segments": alleys}),
                );
                detail_features.extend(fences);
            }
            None => eprintln!("fightbox: no LiDAR for this area; --fences skipped"),
        }
    }
    if options.rail {
        let osm: Value =
            serde_json::from_slice(&raw).map_err(|error| CliError::new(error.to_string()))?;
        let corners = [
            projection.project(bounds[1], bounds[0]),
            projection.project(bounds[3], bounds[2]),
        ];
        let (rail, summary) = detail::rail(
            &osm,
            projection,
            corners,
            lidar.as_ref().map(|measured| &measured.rasters),
        );
        eprintln!("fightbox: rail detail {summary}");
        city_detail.insert("rail".into(), summary);
        detail_features.extend(rail);
    }
    geojson["features"]
        .as_array_mut()
        .expect("converted features")
        .extend(detail_features);
    eprintln!(
        "fightbox: heights {:.1}% LiDAR ({}), {:.1}% roof parts ({}), {:.1}% OSM height/levels ({}), {} default of {} buildings; {} street segments; {}",
        heights["lidar_share_percent"].as_f64().unwrap_or(0.0),
        heights["lidar_count"],
        heights["parts_share_percent"].as_f64().unwrap_or(0.0),
        heights["parts_count"],
        heights["osm_share_percent"].as_f64().unwrap_or(0.0),
        heights["osm_count"],
        heights["default_count"],
        heights["building_count"],
        streets.len(),
        city_place::ATTRIBUTION
    );
    let mut build_metadata = projection.metadata();
    build_metadata["schema_version"] = json!(BUILD_SCHEMA);
    build_metadata["place"] = json!(options.place);
    build_metadata["display_name"] = json!(display_name);
    build_metadata["bbox_south_west_north_east"] = json!(bounds);
    build_metadata["query_text"] = query_text;
    build_metadata["overpass"] = input_metadata;
    build_metadata["overpass_parts"] =
        parts_response.map_or(Value::Null, |response| response.metadata);
    build_metadata["nominatim"] = geocoding;
    build_metadata["street_lookup"] = street_lookup;
    let raw_data: Value =
        serde_json::from_slice(&raw).map_err(|error| CliError::new(error.to_string()))?;
    build_metadata["data_timestamps"] = json!({"osm_base": raw_data.pointer("/osm3s/timestamp_osm_base"),
        "overpass_fetched_at_unix_s": build_metadata["overpass"]["fetched_at_unix_s"],
        "nominatim_fetched_at_unix_s": build_metadata["nominatim"]["fetched_at_unix_s"]});
    build_metadata["attribution"] = match (&lidar, assessor_available) {
        (Some(_), false) => json!([city_place::ATTRIBUTION, lidar::ATTRIBUTION]),
        (Some(_), true) => json!([city_place::ATTRIBUTION, lidar::ATTRIBUTION, assessor::ATTRIBUTION]),
        (None, true) => json!([city_place::ATTRIBUTION, assessor::ATTRIBUTION]),
        (None, false) => json!(city_place::ATTRIBUTION),
    };
    build_metadata["height_coverage"] = heights.clone();
    build_metadata["city_detail"] = Value::Object(city_detail);
    build_metadata["probe_mask"] = if probe_mask.is_empty() {
        Value::Null
    } else {
        city::probe_mask_identity(&probe_mask)
    };
    build_metadata["height_source"] = lidar
        .as_ref()
        .map_or(Value::Null, |measured| measured.metadata.clone());
    write_json_atomic(&marker, &build_metadata)?;
    write_json_atomic(
        &output.join("streets.json"),
        &json!({"attribution": city_place::ATTRIBUTION,
        "origin": build_metadata["origin"], "streets": streets.iter().map(|street|
            json!({"osm_id": street.id, "name": street.name, "highway": street.highway,
                "service": street.service, "points_m": street.points})).collect::<Vec<_>>()}),
    )?;
    let geojson_path = output.join("city.geojson");
    write_json_atomic(&geojson_path, &geojson)?;
    record_stage(&mut stages, "convert", stage, false);

    let stage = Instant::now();
    let package = output.join("city.fightbox");
    let staged_package = AtomicDir::create(output.join(".package-next"))?;
    city::compile_osm_geojson(&geojson_path, staged_package.temp_path(), projection)?;
    record_stage(&mut stages, "compile", stage, false);

    let stage = Instant::now();
    let package_hash = city::package_hash(staged_package.temp_path())?;
    let identity = cache_identity(&package_hash, &options.bake, &probe_mask);
    let cache_key = sha256_hex(
        &serde_json::to_vec(&identity).map_err(|error| CliError::new(error.to_string()))?,
    );
    let baked = output.join("bakes").join(format!("{cache_key}.baked"));
    let (estimate, area_m2) =
        city::bake_estimate_masked(staged_package.temp_path(), &options.bake, &probe_mask)?;
    let (probe_bound, estimated_bytes) = (estimate.probe_count_upper_bound, estimate.bytes);
    if !probe_mask.is_empty() {
        let (all_probes, _, all_bytes) =
            city::bake_estimate(staged_package.temp_path(), &options.bake)?;
        eprintln!(
            "fightbox: probe mask keeps {probe_bound} of {all_probes} probes ({:.0}%){}{}; bake estimate {} instead of {}, ~{:.0} s instead of ~{:.0} s",
            100.0 * probe_bound as f64 / all_probes.max(1) as f64,
            if probe_mask.drop_over_solids { ", buildings dropped" } else { "" },
            probe_mask.corridor.as_ref().map_or(String::new(), |corridor| format!(
                ", corridor {} m of {} route(s) + {} island(s)",
                corridor.half_width_m,
                corridor.routes_enu_m.len(),
                corridor.islands_enu_m.len()
            )) + &probe_mask.graded.as_ref().map_or(String::new(), |graded| format!(
                ", graded: {} m lattice beyond {} walked disk(s) and {} route(s)",
                options.bake.probe_spacing_m * graded.coarse_stride as f32,
                graded.fine.islands_enu_m.len(),
                graded.fine.routes_enu_m.len()
            )),
            crate::bake_reservation::format_bytes(estimated_bytes),
            crate::bake_reservation::format_bytes(all_bytes),
            bake_planning_seconds(probe_bound, &options.bake),
            bake_planning_seconds(all_probes, &options.bake),
        );
    }
    eprintln!(
        "fightbox: preflight at most {probe_bound} probes, {area_m2:.0} m², reserved artifact estimate {}",
        crate::bake_reservation::format_bytes(estimated_bytes)
    );
    eprintln!(
        "fightbox: bake planning estimate ~{:.0} s (Chicago Loop 150/250 m calibration at 4 threads; default settings, topology and host dependent)",
        bake_planning_seconds(probe_bound, &options.bake)
    );
    let memory = BakeGuard {
        budget: MemoryBudget::for_host(options.max_bake_memory_gib),
        staging_roots: vec![output.clone()],
        lock: Some(lock_path),
    };
    eprintln!("fightbox: {}", memory.budget.describe(estimate.probe_pairs));
    guard_size(probe_bound, area_m2, options.allow_large_bake)?;
    eprintln!(
        "fightbox: floor spacing {} m preserves street coverage; height {} m follows the listener, ceiling {} m excludes rooftops by default.",
        options.bake.probe_spacing_m,
        options.bake.probe_height_above_floor_m,
        options.bake.probe_ceiling_m
    );
    eprintln!(
        "fightbox: path range {} m / visibility {} m follow the retained street candidate; {} sample(s) / {} threshold retain city bake defaults.",
        options.bake.path_range_m,
        options.bake.visibility_range_m,
        options.bake.visibility_samples,
        options.bake.visibility_threshold
    );
    eprintln!(
        "fightbox: no elevated probes by default: street scenes need no airborne layer; {} bake thread(s) limit foreground contention; requested layers {:?}.",
        options.bake.bake_threads, options.bake.elevated_probe_layers_m
    );
    let cache_hit = !options.force_rebake && cache_matches(&baked, &identity);
    record_stage(&mut stages, "preflight", stage, false);

    let stage = Instant::now();
    if cache_hit {
        eprintln!("fightbox: bake cache hit; skipping Steam bake ({cache_key})");
    } else {
        let staged_bake = AtomicDir::create(output.join(".bake-next"))?;
        let destination = staged_bake.temp_path().join("baked");
        let percent = AtomicU32::new(0);
        let progress = |fraction: f32| {
            let next = (fraction.clamp(0.0, 1.0) * 100.0) as u32;
            let previous = percent.fetch_max(next, Ordering::Relaxed);
            if next / 5 > previous / 5 {
                eprintln!(
                    "fightbox: bake progress {next}% ({:.1}s)",
                    stage.elapsed().as_secs_f64()
                );
            }
        };
        city::bake_masked(
            staged_package.temp_path(),
            &destination,
            options.bake.clone(),
            &probe_mask,
            Some(&progress),
            &memory,
        )?;
        let manifest_path = destination.join("city-bake-manifest.json");
        let mut manifest = read_json(&manifest_path)?;
        manifest["cache_identity"] = identity;
        manifest["cache_key_sha256"] = json!(cache_key);
        write_json_atomic(&manifest_path, &manifest)?;
        replace_directory(&destination, &baked)?;
    }
    record_stage(&mut stages, "bake", stage, cache_hit);

    let stage = Instant::now();
    let batch = city::load_baked(&baked)?;
    let positions = batch
        .probe_coverage()
        .map_err(|error| CliError::new(error.to_string()))?
        .spheres()
        .map(|(center, _)| [center.x, center.y, center.z])
        .collect::<Vec<_>>();
    write_bytes_atomic(
        &output.join("probes.json"),
        &serde_json::to_vec(&probes_sidecar(&cache_key, projection.metadata(), &positions))
            .map_err(|error| CliError::new(error.to_string()))?,
    )?;
    let loaded = fightbox_world::read_package(staged_package.temp_path())
        .map_err(|error| CliError::new(format!("cannot load spawn geometry: {error}")))?;
    let (spawn, source, forward, spots) = if streets.is_empty() {
        if options.place.is_some() || options.center.is_some() {
            return Err(CliError::new(
                "map area has no usable street centerlines; choose a nearby --center",
            ));
        }
        let (spawn, source, spots) = street_positions(&batch, &options.bake, &loaded.mesh)?;
        (spawn, source, [0.0, 1.0, 0.0], spots)
    } else {
        let coverage = batch
            .probe_coverage()
            .map_err(|error| CliError::new(error.to_string()))?;
        select_street_positions(
            &streets,
            options.bake.probe_height_above_floor_m,
            |point| {
                coverage.contains(fightbox_steam_audio::EnuVector3 {
                    x: point[0],
                    y: point[1],
                    z: point[2],
                }) && outside_mesh_footprints(point, &loaded.mesh)
            },
            |point| outside_mesh_footprints(point, &loaded.mesh),
        )?
    };
    replace_directory(staged_package.temp_path(), &package)?;
    let fixture_path = output.join("workbench.json");
    write_json_atomic(
        &fixture_path,
        &fixture(spawn, source, forward, &spots, &streets, &options.bake),
    )?;
    // Realistic SPL needs the +30 dB listening monitor to be audible. Seed
    // the workbench sidecar once and never overwrite the user's saved mix.
    let mix_path = output.join("workbench.user.json");
    if !mix_path.exists() {
        write_json_atomic(
            &mix_path,
            &json!({"schema_version": 1, "monitor_gain_db": 30.0, "sources": []}),
        )?;
    }
    record_stage(&mut stages, "fixture", stage, false);

    let stage = Instant::now();
    let executable = std::env::current_exe().map_err(|error| CliError::new(error.to_string()))?;
    let binary = executable.with_file_name("fightbox-workbench");
    let slug = options
        .place
        .as_deref()
        .map(city_place::slug)
        .unwrap_or_else(|| {
            if options.center.is_some() {
                "city".into()
            } else {
                "workbench".into()
            }
        });
    let launcher_path = output.join(format!("run-{slug}.command"));
    let launcher = format!(
        "#!/bin/zsh\nset -e\necho '© OpenStreetMap contributors, ODbL'\necho 'Press Play all or tap a sound. WASD walks. The Music menu plays a song or any app.'\nexec {} --package {} --baked {} --fixture {} --start-audio\n",
        shell_quote(&binary),
        shell_quote(&package),
        shell_quote(&baked),
        shell_quote(&fixture_path)
    );
    write_bytes_atomic(&launcher_path, launcher.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| CliError::new(format!("cannot make launcher executable: {error}")))?;
    }
    record_stage(&mut stages, "launcher", stage, false);
    let total = started.elapsed().as_secs_f64();
    let baked_bytes = directory_bytes(&baked)?;
    build_metadata["listener_position_m"] = json!(spawn);
    build_metadata["source_position_m"] = json!(source);
    build_metadata["launcher"] = json!(launcher_path);
    write_json_atomic(&marker, &build_metadata)?;
    write_json_atomic(
        &output.join("timings.json"),
        &json!({
            "schema_version": BUILD_SCHEMA, "stages": stages, "total_wall_s": total,
            "probe_count": batch.metadata.probe_count, "probe_count_upper_bound": probe_bound,
            "area_m2": area_m2, "estimated_artifact_bytes": estimated_bytes,
            "estimated_bake_wall_s": bake_planning_seconds(probe_bound, &options.bake),
            "package_content_sha256": package_hash, "cache_key_sha256": cache_key,
            "cache_hit": cache_hit, "baked_directory": baked, "launcher": launcher_path,
            "listener_position_m": spawn, "source_position_m": source,
            "triangle_count": loaded.manifest.triangle_count, "baked_bytes": baked_bytes,
            "height_coverage": heights, "map_cache_hit": input_cache_hit,
        }),
    )?;
    eprintln!(
        "fightbox: playable city ready: {} ({} probes, total {total:.3}s)",
        launcher_path.display(),
        batch.metadata.probe_count
    );
    Ok(())
}

/// Kept probe positions read back from the bake, for map overlays. A sidecar
/// only: neither the bake key nor the package includes it.
fn probes_sidecar(cache_key: &str, frame: Value, positions: &[[f32; 3]]) -> Value {
    let centimetres = |value: f32| (f64::from(value) * 100.0).round() / 100.0;
    json!({
        "schema_version": PROBES_SCHEMA,
        "bake_key_sha256": cache_key,
        "frame": frame,
        "count": positions.len(),
        "positions_enu_m": positions.iter().map(|point| point.map(centimetres)).collect::<Vec<_>>(),
    })
}

struct BuildLock(PathBuf);
impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn record_stage(stages: &mut Vec<Value>, name: &str, started: Instant, skipped: bool) {
    let wall_s = started.elapsed().as_secs_f64();
    eprintln!(
        "fightbox: {name} wall={wall_s:.3}s{}",
        if skipped { " (skipped: cache hit)" } else { "" }
    );
    stages.push(json!({"stage": name, "wall_s": wall_s, "skipped": skipped}));
}

fn read_json(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| CliError::new(format!("invalid {}: {error}", path.display())))
}

fn replace_directory(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(
        destination
            .parent()
            .expect("generated directory has a parent"),
    )
    .map_err(|error| CliError::new(error.to_string()))?;
    let backup = destination.with_extension("previous");
    if backup.exists() {
        return Err(CliError::new(format!(
            "interrupted replacement remains at {}; preserve it before retrying",
            backup.display()
        )));
    }
    let existing = destination.exists();
    if existing {
        std::fs::rename(destination, &backup).map_err(|error| CliError::new(error.to_string()))?;
    }
    if let Err(error) = std::fs::rename(source, destination) {
        if existing {
            let _ = std::fs::rename(&backup, destination);
        }
        return Err(CliError::new(format!(
            "cannot install {}: {error}",
            destination.display()
        )));
    }
    if existing {
        std::fs::remove_dir_all(backup).map_err(|error| CliError::new(error.to_string()))?;
    }
    Ok(())
}

fn cache_identity(package_hash: &str, config: &BakeConfig, mask: &ProbeMask) -> Value {
    let mut identity = json!({
        "schema_version": "fightbox.city-build-cache.v1",
        "package_content_sha256": package_hash,
        "bake_config": city::city_bake_manifest("", "", "", config, 0.0)["bake_config"],
        "steam_audio_version": STEAM_AUDIO_VERSION, "upstream_commit": STEAM_AUDIO_UPSTREAM_COMMIT,
    });
    // Unmasked keys stay byte-identical so pre-mask bakes remain cache hits.
    if !mask.is_empty() {
        identity["probe_mask"] = city::probe_mask_identity(mask);
    }
    identity
}

fn cache_matches(directory: &Path, identity: &Value) -> bool {
    let Ok(manifest) = read_json(&directory.join("city-bake-manifest.json")) else {
        return false;
    };
    if manifest["cache_identity"] != *identity
        || manifest["package_content_sha256"] != identity["package_content_sha256"]
        || manifest["bake_config"] != identity["bake_config"]
    {
        return false;
    }
    let Ok(batch) = city::load_baked(directory) else {
        return false;
    };
    manifest["probe_batch_sha256"].as_str() == Some(batch.metadata.content_sha256.as_str())
}

struct Street {
    id: String,
    name: String,
    highway: String,
    /// OSM `service=*` (`alley`, `driveway`, …); empty when untagged.
    service: String,
    points: Vec<[f32; 2]>,
}

fn geojson_bounds(geojson: &Value) -> Result<[f64; 4]> {
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for feature in geojson["features"].as_array().expect("converted features") {
        for ring in feature["geometry"]["coordinates"]
            .as_array()
            .expect("converted rings")
        {
            for point in ring.as_array().expect("converted points") {
                let lon = point[0].as_f64().expect("converted longitude");
                let lat = point[1].as_f64().expect("converted latitude");
                bounds[0] = bounds[0].min(lat);
                bounds[1] = bounds[1].min(lon);
                bounds[2] = bounds[2].max(lat);
                bounds[3] = bounds[3].max(lon);
            }
        }
    }
    if bounds.iter().any(|value| !value.is_finite()) {
        return Err(CliError::new("building geometry has no geographic bounds"));
    }
    Ok(bounds)
}

fn height_coverage(geojson: &Value, measured: Option<&BTreeMap<String, f64>>) -> Value {
    let buildings = geojson["features"].as_array().expect("converted features");
    let total = buildings.len();
    let source = |feature: &Value| {
        feature["properties"]["height_source"]
            .as_str()
            .unwrap_or_else(|| {
                if feature["id"]
                    .as_str()
                    .is_some_and(|id| measured.is_some_and(|heights| heights.contains_key(id)))
                {
                    "lidar"
                } else {
                    "osm"
                }
            })
            .to_owned()
    };
    let lidar = buildings
        .iter()
        .filter(|feature| source(feature) == "lidar")
        .count();
    let parts = buildings
        .iter()
        .filter(|feature| source(feature) == "building_parts")
        .count();
    let features = buildings
        .iter()
        .filter(|feature| !matches!(source(feature).as_str(), "lidar" | "building_parts"))
        .collect::<Vec<_>>();
    let explicit = features
        .iter()
        .filter(|feature| feature["properties"]["height"].is_number())
        .count();
    let levels = features
        .iter()
        .filter(|feature| {
            !feature["properties"]["height"].is_number()
                && feature["properties"]["levels"].is_number()
        })
        .count();
    let share = |count: usize| count as f64 * 100.0 / total.max(1) as f64;
    let fallbacks = buildings
        .iter()
        .filter_map(|feature| {
            feature["properties"]
                .get("osm_plausibility_fallback")
                .map(|fallback| {
                    json!({"building_id": feature["id"], "name": feature["properties"]["name"],
                "height_m": feature["properties"]["height"], "evidence": fallback})
                })
        })
        .collect::<Vec<_>>();
    json!({"building_count": total, "lidar_count": lidar, "lidar_share_percent": share(lidar),
        "parts_count": parts, "parts_share_percent": share(parts),
        "explicit_height_count": explicit, "levels_count": levels,
        "osm_count": explicit + levels, "default_count": features.len() - explicit - levels,
        "osm_share_percent": share(explicit + levels), "osm_plausibility_fallbacks": fallbacks})
}

fn measured_heights(
    options: &BuildOptions,
    output: &Path,
    geojson: &Value,
    requested_bbox: Option<[f64; 4]>,
) -> Result<Option<lidar::Measured>> {
    let footprints = geojson_bounds(geojson)?;
    match options.heights {
        HeightSource::Osm => return Ok(None),
        HeightSource::Auto
            if options.osm.is_some() || !requested_bbox.is_some_and(lidar::covers) =>
        {
            return Ok(None);
        }
        HeightSource::Lidar if !lidar::covers(footprints) => {
            return Err(CliError::new(
                "--heights lidar covers Cook County, Illinois only; use --heights osm elsewhere",
            ));
        }
        _ => {}
    }
    match lidar::measure(output, geojson, footprints) {
        Ok(measured) => Ok(Some(measured)),
        Err(error) if options.heights == HeightSource::Auto => {
            eprintln!("fightbox: LiDAR heights unavailable ({error}); keeping OSM height/levels");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn osm_streets(raw: &[u8], projection: Projection, bbox: [f64; 4]) -> Result<Vec<Street>> {
    let root: Value =
        serde_json::from_slice(raw).map_err(|error| CliError::new(error.to_string()))?;
    let elements = root["elements"]
        .as_array()
        .ok_or_else(|| CliError::new("OSM elements missing"))?;
    let nodes = elements
        .iter()
        .filter(|element| element["type"] == "node")
        .map(|node| {
            (
                node["id"].as_i64().expect("validated node id"),
                json!([node["lon"], node["lat"]]),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let min = projection
        .project(bbox[1], bbox[0])
        .map(|value| value as f32);
    let max = projection
        .project(bbox[3], bbox[2])
        .map(|value| value as f32);
    let mut streets = Vec::new();
    for way in elements
        .iter()
        .filter(|element| element["type"] == "way" && element["tags"]["highway"].is_string())
    {
        // Elevated roads cannot provide a street-level spawn.
        if way["tags"]["bridge"] == "yes"
            || way["tags"]["tunnel"] == "yes"
            || matches!(
                way["tags"]["highway"].as_str(),
                Some("motorway" | "motorway_link" | "trunk" | "trunk_link")
            )
            || matches!(way["tags"]["access"].as_str(), Some("no" | "private"))
            || way["tags"]["layer"]
                .as_str()
                .is_some_and(|layer| layer != "0")
        {
            continue;
        }
        let points = ring(way, &nodes)?
            .iter()
            .map(|point| {
                projection
                    .project(
                        point[0].as_f64().expect("validated longitude"),
                        point[1].as_f64().expect("validated latitude"),
                    )
                    .map(|value| value as f32)
            })
            .collect::<Vec<_>>();
        for segment in points.windows(2) {
            if let Some([a, b]) = clip_segment(segment[0], segment[1], min, max) {
                if (b[0] - a[0]).hypot(b[1] - a[1]) < 0.1 {
                    continue;
                }
                streets.push(Street {
                    id: format!("way/{}", way["id"]),
                    name: way["tags"]["name"].as_str().unwrap_or("").into(),
                    highway: way["tags"]["highway"]
                        .as_str()
                        .expect("filtered highway")
                        .into(),
                    service: way["tags"]["service"].as_str().unwrap_or("").into(),
                    points: vec![a, b],
                });
            }
        }
    }
    Ok(streets)
}

fn clip_segment(a: [f32; 2], b: [f32; 2], min: [f32; 2], max: [f32; 2]) -> Option<[[f32; 2]; 2]> {
    let mut low = 0.0_f32;
    let mut high = 1.0_f32;
    for axis in 0..2 {
        let delta = b[axis] - a[axis];
        if delta.abs() < 1e-6 {
            if a[axis] < min[axis] || a[axis] > max[axis] {
                return None;
            }
        } else {
            let t0 = (min[axis] - a[axis]) / delta;
            let t1 = (max[axis] - a[axis]) / delta;
            low = low.max(t0.min(t1));
            high = high.min(t0.max(t1));
            if low > high {
                return None;
            }
        }
    }
    let at = |t: f32| [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])];
    Some([at(low), at(high)])
}

/// Spawns on the public road nearest the place and puts the speaker 6-30 m
/// away in plain sight. Alleys and service drives are hemmed in by walls a
/// few metres away that hide every source, so the first listen would be
/// flutter echoes instead of the scene.
fn select_street_positions(
    streets: &[Street],
    height: f32,
    walkable: impl Fn([f32; 3]) -> bool,
    open: impl Fn([f32; 3]) -> bool,
) -> Result<([f32; 3], [f32; 3], [f32; 3], Vec<[f32; 3]>)> {
    let mut candidates = Vec::new();
    for street in streets {
        for segment in street.points.windows(2) {
            let [a, b] = [segment[0], segment[1]];
            let delta = [b[0] - a[0], b[1] - a[1]];
            let length = delta[0].hypot(delta[1]);
            if length < 0.1 {
                continue;
            }
            let steps = length.ceil() as usize;
            let nearest = (-(a[0] * delta[0] + a[1] * delta[1]) / length.powi(2)).clamp(0.0, 1.0);
            let road = !matches!(
                street.highway.as_str(),
                "footway" | "path" | "steps" | "cycleway" | "service" | "track" | "pedestrian"
            );
            for t in (0..=steps)
                .map(|step| step as f32 / steps as f32)
                .chain([nearest])
            {
                let point = [a[0] + t * delta[0], a[1] + t * delta[1], height];
                if walkable(point) {
                    candidates.push((
                        point,
                        [delta[0] / length, delta[1] / length, 0.0],
                        t == 0.0 || t == 1.0,
                        road,
                    ));
                }
            }
        }
    }
    let spawn = candidates
        .iter()
        .min_by(|a, b| {
            let score = |candidate: &([f32; 3], [f32; 3], bool, bool)| {
                candidate.0[0].hypot(candidate.0[1]) + if candidate.3 { 0.0 } else { 60.0 }
            };
            score(a).total_cmp(&score(b))
        })
        .ok_or_else(|| CliError::new("no covered street-level spawn outside buildings"))?;
    let mut speakers = candidates
        .iter()
        .filter(|candidate| {
            let distance = (candidate.0[0] - spawn.0[0]).hypot(candidate.0[1] - spawn.0[1]);
            (6.0..=30.0).contains(&distance)
        })
        .collect::<Vec<_>>();
    speakers.sort_by(|a, b| {
        let score = |candidate: &([f32; 3], [f32; 3], bool, bool)| {
            let distance = (candidate.0[0] - spawn.0[0]).hypot(candidate.0[1] - spawn.0[1]);
            (distance - 15.0).abs() + if candidate.2 { 0.0 } else { 20.0 }
        };
        score(a).total_cmp(&score(b))
    });
    // Metre steps along the sight line; a speaker behind a corner falls back
    // only when nothing in range is visible.
    let in_sight = |target: [f32; 3]| {
        let steps = (target[0] - spawn.0[0]).hypot(target[1] - spawn.0[1]).ceil() as usize;
        (1..steps).all(|step| {
            let t = step as f32 / steps as f32;
            open([
                spawn.0[0] + t * (target[0] - spawn.0[0]),
                spawn.0[1] + t * (target[1] - spawn.0[1]),
                height,
            ])
        })
    };
    let source = speakers
        .iter()
        .find(|candidate| in_sight(candidate.0))
        .or(speakers.first())
        .ok_or_else(|| CliError::new("no covered street speaker position within 30 m of spawn"))?;
    let spots = candidates.iter().map(|candidate| candidate.0).collect();
    Ok((spawn.0, source.0, spawn.1, spots))
}

fn outside_mesh_footprints(point: [f32; 3], mesh: &fightbox_world::AcousticMesh) -> bool {
    !mesh.triangles.iter().any(|triangle| {
        let vertices = triangle.map(|index| mesh.vertices_enu_m[index as usize]);
        vertices.iter().all(|vertex| vertex.up_m > 0.0)
            && point_in_triangle(
                [point[0], point[1]],
                vertices.map(|v| [v.east_m, v.north_m]),
            )
    })
}

fn directory_bytes(path: &Path) -> Result<u64> {
    let mut bytes = 0;
    for entry in std::fs::read_dir(path).map_err(|error| CliError::new(error.to_string()))? {
        let entry = entry.map_err(|error| CliError::new(error.to_string()))?;
        let metadata = entry
            .metadata()
            .map_err(|error| CliError::new(error.to_string()))?;
        bytes += if metadata.is_dir() {
            directory_bytes(&entry.path())?
        } else {
            metadata.len()
        };
    }
    Ok(bytes)
}

fn street_positions(
    batch: &fightbox_steam_audio::BakedProbeBatch,
    config: &BakeConfig,
    mesh: &fightbox_world::AcousticMesh,
) -> Result<([f32; 3], [f32; 3], Vec<[f32; 3]>)> {
    let coverage = batch
        .probe_coverage()
        .map_err(|error| CliError::new(format!("cannot read street coverage: {error}")))?;
    let positions = coverage
        .spheres()
        .map(|(center, _)| center)
        .filter(|center| (center.z - config.probe_height_above_floor_m).abs() < 0.1)
        .filter(|center| {
            !mesh.triangles.iter().any(|triangle| {
                let vertices = triangle.map(|index| mesh.vertices_enu_m[index as usize]);
                vertices.iter().all(|vertex| vertex.up_m > 0.0)
                    && point_in_triangle(
                        [center.x, center.y],
                        vertices.map(|vertex| [vertex.east_m, vertex.north_m]),
                    )
            })
        })
        .collect::<Vec<_>>();
    if positions.len() < 2 {
        return Err(CliError::new(
            "city bake needs two street-level probes for a walkable scene",
        ));
    }
    let center = positions.iter().fold([0.0_f32; 2], |sum, point| {
        [sum[0] + point.x, sum[1] + point.y]
    });
    let target = [
        center[0] / positions.len() as f32,
        center[1] / positions.len() as f32,
    ];
    let spawn = positions
        .iter()
        .min_by(|a, b| {
            let distance = |p: &fightbox_steam_audio::EnuVector3| {
                (p.x - target[0]).powi(2) + (p.y - target[1]).powi(2)
            };
            distance(a).total_cmp(&distance(b))
        })
        .expect("nonempty positions");
    let source = positions
        .iter()
        .filter(|p| p.x != spawn.x || p.y != spawn.y)
        .min_by(|a, b| {
            let distance = |p: &fightbox_steam_audio::EnuVector3| {
                (((p.x - spawn.x).powi(2) + (p.y - spawn.y).powi(2)).sqrt() - 8.0).abs()
            };
            distance(a).total_cmp(&distance(b))
        })
        .expect("distinct probe positions");
    let spots = positions.iter().map(|p| [p.x, p.y, p.z]).collect();
    Ok((
        [spawn.x, spawn.y, spawn.z],
        [source.x, source.y, source.z],
        spots,
    ))
}

fn point_in_triangle(point: [f32; 2], vertices: [[f32; 2]; 3]) -> bool {
    let cross = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    };
    if cross(vertices[0], vertices[1], vertices[2]).abs() < 1e-5 {
        return false;
    }
    let signs = [
        cross(vertices[0], vertices[1], point),
        cross(vertices[1], vertices[2], point),
        cross(vertices[2], vertices[0], point),
    ];
    signs.iter().all(|value| *value >= -1e-5) || signs.iter().all(|value| *value <= 1e-5)
}

/// The street spot whose distance from `from` is nearest `metres`.
fn spot_near(spots: &[[f32; 3]], from: [f32; 3], metres: f32) -> Option<[f32; 3]> {
    spots.iter().copied().min_by(|a, b| {
        let error = |p: &[f32; 3]| ((p[0] - from[0]).hypot(p[1] - from[1]) - metres).abs();
        error(a).total_cmp(&error(b))
    })
}

/// A road near, but not through, the listener, driven out and back so the
/// route stays on the street. Points beyond `reach` are dropped.
fn siren_route(streets: &[Street], spawn: [f32; 3], reach: f32) -> Option<Vec<[f32; 3]>> {
    let distance = |street: &Street| {
        street
            .points
            .windows(2)
            .map(|segment| segment_distance([spawn[0], spawn[1]], segment[0], segment[1]))
            .fold(f32::INFINITY, f32::min)
    };
    let road = |street: &Street| {
        !street.points.is_empty()
            && !matches!(
                street.highway.as_str(),
                "footway" | "path" | "steps" | "cycleway" | "service"
            )
    };
    // A through street a block or two away; residential streets only as a fallback.
    let score = |street: &Street| {
        let arterial = matches!(
            street.highway.as_str(),
            "primary" | "secondary" | "tertiary"
        );
        distance(street) + if arterial { 0.0 } else { 60.0 }
    };
    let (first, start) = streets
        .iter()
        .enumerate()
        .filter(|(_, street)| road(street) && (12.0..=150.0).contains(&distance(street)))
        .min_by(|a, b| score(a.1).total_cmp(&score(b.1)))?;
    // OSM splits a street into pieces; rejoin pieces with the same name.
    let mut line = start.points.clone();
    let mut used = vec![first];
    let meets = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]) < 1.0;
    while !start.name.is_empty() {
        let next = streets.iter().enumerate().find(|(index, street)| {
            road(street)
                && street.name == start.name
                && !used.contains(index)
                && [line[0], line[line.len() - 1]].iter().any(|end| {
                    meets(*end, street.points[0])
                        || meets(*end, street.points[street.points.len() - 1])
                })
        });
        let Some((index, next)) = next else { break };
        used.push(index);
        let mut piece = next.points.clone();
        if meets(line[line.len() - 1], piece[piece.len() - 1]) || meets(line[0], piece[0]) {
            piece.reverse();
        }
        if meets(line[line.len() - 1], piece[0]) {
            line.extend_from_slice(&piece[1..]);
        } else {
            piece.extend_from_slice(&line[1..]);
            line = piece;
        }
    }
    let mut points = line
        .iter()
        .filter(|p| (p[0] - spawn[0]).hypot(p[1] - spawn[1]) <= reach)
        .map(|p| [p[0], p[1], 1.5])
        .collect::<Vec<_>>();
    if points.len() < 2 {
        return None;
    }
    let back = points[1..points.len() - 1]
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>();
    points.extend(back);
    Some(points)
}

fn segment_distance(point: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let delta = [b[0] - a[0], b[1] - a[1]];
    let length2 = delta[0] * delta[0] + delta[1] * delta[1];
    let t = if length2 > 0.0 {
        (((point[0] - a[0]) * delta[0] + (point[1] - a[1]) * delta[1]) / length2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (a[0] + t * delta[0] - point[0]).hypot(a[1] + t * delta[1] - point[1])
}

/// The combat reference's 155 mm shell (last 3 km at Mach 1.5, 45 degree
/// dive), arriving from behind the listener and 45 degrees aside so its
/// crack passes them before the impact.
fn shell_flight(spawn: [f32; 3], impact: [f32; 3]) -> Value {
    const FLIGHT_M: f32 = 3000.0;
    let bearing = (impact[0] - spawn[0]).atan2(impact[1] - spawn[1]) + std::f32::consts::FRAC_PI_4;
    let reach = FLIGHT_M * std::f32::consts::FRAC_1_SQRT_2;
    let muzzle = [
        impact[0] - reach * bearing.sin(),
        impact[1] - reach * bearing.cos(),
        impact[2] + reach,
    ];
    let length = ((impact[0] - muzzle[0]).powi(2)
        + (impact[1] - muzzle[1]).powi(2)
        + (impact[2] - muzzle[2]).powi(2))
    .sqrt();
    json!({
        "muzzle_position_m": muzzle,
        "mach_segments": [{"length_m": length, "mach": 1.5}],
        "n_wave_ms_at_30_m": 2.8,
        "crack_peak_db_at_30_m": 150.8,
    })
}

/// The repository's demo song, when this checkout has the local asset.
fn demo_song() -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/assets/music/toms-diner-48k-mono.wav")
        .canonicalize()
        .ok()?;
    Some(path.to_string_lossy().into_owned())
}

fn fixture(
    spawn: [f32; 3],
    speaker: [f32; 3],
    forward: [f32; 3],
    spots: &[[f32; 3]],
    streets: &[Street],
    config: &BakeConfig,
) -> Value {
    // Start facing the music speaker, so it is the first thing in view.
    let toward = [speaker[0] - spawn[0], speaker[1] - spawn[1]];
    let length = toward[0].hypot(toward[1]);
    let facing = if length > 0.5 {
        [toward[0] / length, toward[1] / length, 0.0]
    } else {
        forward
    };
    let mut music = json!({"device": "All system audio", "channels": "stereo"});
    if let Some(song) = demo_song() {
        music["song"] = json!(song);
    }
    let mut sources = vec![json!({"id": "music", "audition_label": "Music",
        "live_input": music, "extent": {"kind": "stereo_image", "width_m": 4.0},
        "default_enabled": false, "restart_on_enable": true,
        "reference_level": {"mode": "SplAtOneMeter", "db_spl": 115.0},
        "position_m": [speaker[0], speaker[1], 3.0]})];
    if let Some(bells) = spot_near(spots, spawn, 110.0) {
        sources.push(json!({"id": "bells", "audition_label": "Church bells",
            "asset_id": "church-bells", "default_enabled": false, "restart_on_enable": true,
            "reference_level": {"mode": "SplAtOneMeter", "db_spl": 119.0},
            "position_m": [bells[0], bells[1], 25.0]}));
    }
    if let Some(route) = siren_route(streets, spawn, 220.0) {
        sources.push(json!({"id": "siren", "audition_label": "Siren · driving",
            "asset_id": "ff-siren", "default_enabled": false, "restart_on_enable": true,
            "reference_level": {"mode": "SplAtOneMeter", "db_spl": 126.0},
            "directivity": {"dipole_weight": 0.5, "dipole_power": 2.0},
            "trajectory": {"waypoints_m": route, "speed_mps": 12.0, "max_speed_mps": 12.0}}));
    }
    if let Some(artillery) = spot_near(spots, spawn, 200.0) {
        sources.push(
            json!({"id": "artillery", "audition_label": "Artillery · far",
            "asset_id": "astra-artillery-single", "default_enabled": false,
            "restart_on_enable": true, "impulsive": true,
            "extent": {"kind": "line_segment", "length_m": 6.0}, "monitor_offset_db": -16,
            "reference_level": {"mode": "SplAtOneMeter", "db_spl": 155.0},
            "position_m": artillery,
            "ballistic": shell_flight(spawn, artillery)}),
        );
    }
    // A circle whose edge passes straight over the listener once per lap.
    let orbit = (0..24)
        .map(|step| {
            let angle = std::f32::consts::PI + step as f32 * std::f32::consts::TAU / 24.0;
            [
                spawn[0] + 120.0 + 120.0 * angle.cos(),
                spawn[1] + 120.0 * angle.sin(),
                80.0,
            ]
        })
        .collect::<Vec<_>>();
    sources.push(json!({"id": "helicopter", "audition_label": "Helicopter",
        "asset_id": "squad-mi8-rotor-close", "default_enabled": false, "restart_on_enable": true,
        "extent": {"kind": "line_segment", "length_m": 21.0}, "monitor_offset_db": 12,
        "reference_level": {"mode": "SplAtOneMeter", "db_spl": 110.0},
        "trajectory": {"waypoints_m": orbit, "speed_mps": 30.0, "max_speed_mps": 30.0}}));
    json!({
        "fixture_id": "city-built", "air": "temperate",
        "sources": sources,
        "listener": {"position_m": spawn, "forward_enu": facing},
        "street_lines_m": streets.iter().map(|street| &street.points).collect::<Vec<_>>(),
        "street_names": streets.iter().map(|street| &street.name).collect::<Vec<_>>(),
        "street_kinds": streets.iter().map(|street| &street.highway).collect::<Vec<_>>(),
        "simulation": {
            "direct": {"distance_attenuation": true, "occlusion": true, "occlusion_samples": 64},
            "reflections": {"enabled": true, "rays": 4096, "bounces": 8, "duration_s": 1.5},
            "pathing": {"order": 2, "validation": true, "alternate_paths": true, "visibility_range_m": config.visibility_range_m},
            "probe_volume": {"spacing_m": config.probe_spacing_m},
        },
    })
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn numeric_tag(tags: &Value, key: &str, metres: bool) -> Option<Value> {
    let raw = tags.get(key)?.as_str()?.trim();
    let raw = if metres {
        raw.strip_suffix('m').unwrap_or(raw).trim()
    } else {
        raw
    };
    if !metres && !raw.contains('.') {
        return raw.parse::<i64>().ok().map(Value::from);
    }
    raw.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(Value::from)
}

fn properties(tags: &Value) -> Value {
    let mut props = json!({"building": tags["building"]});
    if let Some(name) = tags
        .get("name")
        .filter(|value| value.as_str().is_some_and(|name| !name.is_empty()))
    {
        props["name"] = name.clone();
    }
    if let Some(height) = numeric_tag(tags, "height", true) {
        props["height"] = height;
    }
    if let Some(levels) = numeric_tag(tags, "building:levels", false) {
        props["levels"] = levels;
    }
    if let Some(raw) = tags.get("building:material").and_then(Value::as_str) {
        let material = raw.trim().to_lowercase();
        if MaterialTable::default().get(&material).is_some() {
            props["building:material"] = json!(raw);
            props["material"] = json!(material);
        }
    }
    props
}

fn ring(way: &Value, nodes: &BTreeMap<i64, Value>) -> Result<Vec<Value>> {
    let refs = way["nodes"]
        .as_array()
        .ok_or_else(|| CliError::new("OSM way needs node references"))?;
    refs.iter()
        .map(|reference| {
            let id = reference
                .as_i64()
                .ok_or_else(|| CliError::new("invalid OSM node reference"))?;
            nodes.get(&id).cloned().ok_or_else(|| {
                CliError::new(format!("way {} references missing node {id}", way["id"]))
            })
        })
        .collect()
}

fn feature(id: String, tags: &Value, ring: Vec<Value>) -> Value {
    json!({"type": "Feature", "id": id, "properties": properties(tags),
        "geometry": {"type": "Polygon", "coordinates": [ring]}})
}

fn join_outer_rings(mut parts: Vec<Vec<Value>>) -> Result<Vec<Vec<Value>>> {
    let mut rings = Vec::new();
    for part in &mut parts {
        part.dedup();
    }
    parts.retain(|part| part.len() >= 2);
    while !parts.is_empty() {
        let mut ring = parts.remove(0);
        while ring.first() != ring.last() {
            let end = ring.last().expect("nonempty ring");
            let next = parts
                .iter()
                .position(|part| part.first() == Some(end) || part.last() == Some(end))
                .ok_or_else(|| {
                    CliError::new("building relation outer ways do not form closed rings")
                })?;
            let mut part = parts.remove(next);
            if part.first() != Some(end) {
                part.reverse();
            }
            ring.extend(part.into_iter().skip(1));
        }
        if ring.len() < 4 {
            return Err(CliError::new(
                "building relation ring needs three distinct vertices",
            ));
        }
        rings.push(ring);
    }
    Ok(rings)
}

fn convert_osm(raw: &[u8]) -> Result<Value> {
    convert_osm_with_heights(raw, &BTreeMap::new())
}

// Height providers (LiDAR today) resolve footprint ids into this table; OSM
// height/levels remain the fallback for every footprint they do not measure.
fn convert_osm_with_heights(raw: &[u8], height_overrides: &BTreeMap<String, f64>) -> Result<Value> {
    let data: Value = serde_json::from_slice(raw)
        .map_err(|error| CliError::new(format!("invalid Overpass JSON: {error}")))?;
    let elements = data["elements"]
        .as_array()
        .ok_or_else(|| CliError::new("Overpass response needs elements"))?;
    let mut nodes = BTreeMap::new();
    let mut ways = BTreeMap::new();
    for element in elements {
        let id = element["id"]
            .as_i64()
            .ok_or_else(|| CliError::new("OSM element needs an integer id"))?;
        match element["type"].as_str() {
            Some("node") => {
                let lon = element["lon"]
                    .as_f64()
                    .ok_or_else(|| CliError::new("OSM node needs longitude"))?;
                let lat = element["lat"]
                    .as_f64()
                    .ok_or_else(|| CliError::new("OSM node needs latitude"))?;
                if !(-180.0..=180.0).contains(&lon) || !(-90.0..90.0).contains(&lat) {
                    return Err(CliError::new("OSM node coordinates out of range"));
                }
                nodes.insert(id, json!([lon, lat]));
            }
            Some("way") => {
                ways.insert(id, element);
            }
            _ => {}
        }
    }
    let mut features = Vec::new();
    let mut members = BTreeSet::new();
    let mut inner_holes = 0;
    for relation in elements.iter().filter(|element| {
        element["type"] == "relation" && element["tags"].get("building").is_some()
    }) {
        let mut outer_parts = Vec::new();
        for member in relation["members"]
            .as_array()
            .ok_or_else(|| CliError::new("OSM relation needs members"))?
        {
            if member["type"] != "way" {
                continue;
            }
            let id = member["ref"]
                .as_i64()
                .ok_or_else(|| CliError::new("invalid relation member ref"))?;
            members.insert(id);
            if member["role"] == "inner" {
                inner_holes += 1;
                continue;
            }
            if let Some(way) = ways.get(&id) {
                outer_parts.push(ring(way, &nodes)?);
            } else {
                eprintln!(
                    "fightbox: relation {} missing outer way {id}",
                    relation["id"]
                );
            }
        }
        let rings = join_outer_rings(outer_parts)
            .map_err(|error| CliError::new(format!("relation {}: {error}", relation["id"])))?;
        let multiple = rings.len() > 1;
        for (part, coordinates) in rings.into_iter().enumerate() {
            let id = if multiple {
                format!("relation/{}:{part}", relation["id"])
            } else {
                format!("relation/{}", relation["id"])
            };
            features.push(feature(id, &relation["tags"], coordinates));
        }
    }
    for way in elements
        .iter()
        .filter(|element| element["type"] == "way" && element["tags"].get("building").is_some())
    {
        if members.contains(&way["id"].as_i64().expect("validated id")) {
            continue;
        }
        let refs = way["nodes"]
            .as_array()
            .ok_or_else(|| CliError::new("OSM way needs nodes"))?;
        if refs.is_empty() || refs.first() != refs.last() {
            eprintln!("fightbox: skipping unclosed building way {}", way["id"]);
            continue;
        }
        features.push(feature(
            format!("way/{}", way["id"]),
            &way["tags"],
            ring(way, &nodes)?,
        ));
    }
    if features.is_empty() {
        return Err(CliError::new(
            "Overpass input contains no closed building polygons",
        ));
    }
    eprintln!(
        "fightbox: converted {} buildings; skipped {inner_holes} inner hole members (same policy as convert.py)",
        features.len()
    );
    let mut geojson = json!({"type": "FeatureCollection", "features": features});
    apply_height_overrides(&mut geojson, height_overrides)?;
    Ok(geojson)
}

/// Measured heights replace OSM height/levels unless the measurement is less
/// than half the mapped height and at least 10 m lower. Such large shortfalls
/// are common for poorly classified tall returns; normal roof differences stay
/// measured. Keep both values so the selected acoustic height is auditable.
fn apply_height_overrides(
    geojson: &mut Value,
    height_overrides: &BTreeMap<String, f64>,
) -> Result<()> {
    for feature in geojson["features"]
        .as_array_mut()
        .expect("converted features")
    {
        if let Some(height) = feature["id"].as_str().and_then(|id| {
            height_overrides
                .get(id)
                .or_else(|| height_overrides.get(id.split(':').next().expect("id")))
        }) {
            if !height.is_finite() || *height <= 0.0 || *height > f64::from(f32::MAX) {
                return Err(CliError::new(
                    "height override must be finite, positive metres",
                ));
            }
            let props = &mut feature["properties"];
            let osm = props["height"]
                .as_f64()
                .filter(|height| *height > 0.0)
                .or_else(|| {
                    props["levels"]
                        .as_f64()
                        .filter(|levels| *levels > 0.0)
                        .map(|levels| levels * 3.5)
                });
            props["lidar_height_m"] = json!(height);
            if let Some(osm) = osm.filter(|osm| *height < *osm * 0.5 && *osm - *height >= 10.0) {
                props["height"] = json!(osm);
                props["height_source"] = json!("osm_plausibility_fallback");
                props["osm_plausibility_fallback"] = json!({"lidar_height_m": height, "osm_height_m": osm,
                    "rule": "LiDAR < 50% of OSM and at least 10 m lower; levels use 3.5 m"});
            } else {
                props["height"] = json!(height);
                props["height_source"] = json!("lidar");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lidar_shortfall_uses_osm_height_or_35m_per_ten_levels_and_records_it() {
        let mut geojson = json!({"features": [
            {"id":"height", "properties":{"height":100}},
            {"id":"levels", "properties":{"levels":10}},
            {"id":"house", "properties":{"levels":2}},
            {"id":"healthy", "properties":{"height":260}}
        ]});
        let heights = BTreeMap::from([
            ("height".into(), 20.0),
            ("levels".into(), 12.0),
            ("house".into(), 5.0),
            ("healthy".into(), 246.0),
        ]);
        apply_height_overrides(&mut geojson, &heights).unwrap();
        let features = geojson["features"].as_array().unwrap();
        assert_eq!(features[0]["properties"]["height"], 100.0);
        assert_eq!(features[1]["properties"]["height"], 35.0);
        assert_eq!(features[2]["properties"]["height"], 5.0);
        assert_eq!(features[3]["properties"]["height"], 246.0);
        let coverage = height_coverage(&geojson, Some(&heights));
        assert_eq!(
            coverage["osm_plausibility_fallbacks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(coverage["lidar_count"], 2);
        assert_eq!(coverage["osm_count"], 2);
    }

    #[test]
    fn local_osm_keeps_offline_defaults_and_geographic_osm_opts_out_of_lidar() {
        let local = parse(&[
            "--osm".into(),
            "recorded.json".into(),
            "--output".into(),
            "unused".into(),
        ])
        .unwrap();
        assert_eq!(local.heights, HeightSource::Auto);
        assert!(!local.fences && !local.assessor_materials && !local.rail);
        let geographic = parse(&[
            "--center".into(),
            "0,0".into(),
            "--heights".into(),
            "osm".into(),
            "--output".into(),
            "unused".into(),
        ])
        .unwrap();
        assert_eq!(geographic.heights, HeightSource::Osm);
        assert!(!geographic.fences);
        assert!(geographic.assessor_materials && geographic.rail);
    }

    #[test]
    fn chicago_osm_matches_python_geojson_and_height_policy() {
        let converted = convert_osm(include_bytes!(
            "../../../fixtures/city/chicago-block/raw/overpass_raw.json"
        ))
        .unwrap();
        let python: Value = serde_json::from_slice(include_bytes!(
            "../../../fixtures/city/chicago-block/chicago-block.geojson"
        ))
        .unwrap();
        assert_eq!(converted, python);
        assert_eq!(converted["features"].as_array().unwrap().len(), 4);
        let tags = json!({"building": "yes", "height": "45 m", "building:levels": "13", "building:material": "concrete"});
        let props = properties(&tags);
        assert_eq!(props["height"], 45.0);
        assert_eq!(props["levels"], 13);
        assert_eq!(props["material"], "concrete");
        assert!(
            properties(&json!({"building": "yes", "building:levels": "2.5"}))
                .get("height")
                .is_none()
        );
        assert_eq!(
            properties(&json!({"building": "yes", "building:levels": "2.5"}))["levels"],
            2.5
        );
        assert!(
            properties(&json!({"building": "yes", "building:material": "unknown"}))
                .get("material")
                .is_none()
        );
    }

    #[test]
    fn probes_sidecar_lists_rounded_positions_with_count_key_and_frame() {
        let frame = Projection { center: [41.9, -87.6] }.metadata();
        let sidecar = probes_sidecar("abc123", frame.clone(), &[[1.234_567, -2.0, 1.5], [40.0, 80.005, 1.5]]);
        assert_eq!(sidecar["schema_version"], PROBES_SCHEMA);
        assert_eq!(sidecar["bake_key_sha256"], "abc123");
        assert_eq!(sidecar["frame"], frame);
        assert_eq!(sidecar["count"], 2);
        assert_eq!(sidecar["positions_enu_m"], json!([[1.23, -2.0, 1.5], [40.0, 80.0, 1.5]]));
    }

    #[test]
    fn cache_key_stable_and_invalidated_by_package_config_and_probe_mask() {
        let config = BakeConfig::default();
        let legacy = ProbeMask::default();
        let key = cache_identity("package-a", &config, &legacy);
        assert_eq!(key, cache_identity("package-a", &config.clone(), &legacy));
        assert_ne!(key, cache_identity("package-b", &config, &legacy));
        let mut changed = config.clone();
        changed.visibility_range_m += 1.0;
        assert_ne!(key, cache_identity("package-a", &changed, &legacy));
        changed = config.clone();
        changed.elevated_probe_layers_m.push(30.0);
        assert_ne!(key, cache_identity("package-a", &changed, &legacy));
        changed = config.clone();
        changed.bake_threads += 1;
        assert_ne!(key, cache_identity("package-a", &changed, &legacy));
        // The unmasked key is the pre-mask key; every mask gets its own.
        assert!(key.get("probe_mask").is_none());
        let dropped = ProbeMask { drop_over_solids: true, corridor: None, graded: None };
        let walked = |half_width_m| ProbeMask {
            drop_over_solids: true,
            corridor: Some(fightbox_steam_audio::ProbeCorridor {
                routes_enu_m: vec![vec![[0.0, 0.0], [50.0, 0.0]]],
                half_width_m,
                islands_enu_m: Vec::new(),
            }),
            graded: None,
        };
        let graded = |stride| ProbeMask {
            drop_over_solids: true,
            corridor: None,
            graded: Some(graded::density(graded::GradedOptions::DEFAULT, stride, None, &[])),
        };
        // A pre-graded mask keeps its key: graded enters only when present.
        assert!(cache_identity("package-a", &config, &dropped)["probe_mask"]
            .get("graded")
            .is_none());
        let keys = [
            key,
            cache_identity("package-a", &config, &dropped),
            cache_identity("package-a", &config, &walked(20.0)),
            cache_identity("package-a", &config, &walked(30.0)),
            cache_identity("package-a", &config, &graded(2)),
            cache_identity("package-a", &config, &graded(3)),
        ];
        for (index, first) in keys.iter().enumerate() {
            assert!(keys[index + 1..].iter().all(|other| other != first));
        }
    }

    #[test]
    fn refuses_large_probe_or_area_cost_before_bake() {
        assert!(guard_size(540, 8000.0, false).is_ok());
        assert!(
            guard_size(MAX_PROBES + 1, 8000.0, false)
                .unwrap_err()
                .to_string()
                .contains("--allow-large-bake")
        );
        assert!(guard_size(540, MAX_AREA_M2 + 1.0, false).is_err());
        assert!(guard_size(MAX_PROBES + 1, MAX_AREA_M2 + 1.0, true).is_ok());
        assert!(guard_query_area(4.0 * 250.0_f64.powi(2), false).is_ok());
        assert!(guard_query_area(4.0 * 251.0_f64.powi(2), false).is_err());
        let options = parse(&[
            "--place".into(),
            "Chicago".into(),
            "--output".into(),
            "unused".into(),
        ])
        .unwrap();
        assert_eq!(options.bake.bake_threads, 4);
        assert_eq!(options.heights, HeightSource::Auto);
        assert!(options.fences && options.assessor_materials && options.rail);
        assert!(!options.assessor_required);
        let osm_heights = |value: &str| {
            parse(&[
                "--center".into(),
                "41.96,-87.67".into(),
                "--heights".into(),
                value.into(),
                "--output".into(),
                "unused".into(),
            ])
            .map(|options| options.heights)
        };
        assert_eq!(osm_heights("osm").unwrap(), HeightSource::Osm);
        assert_eq!(osm_heights("lidar").unwrap(), HeightSource::Lidar);
        assert!(osm_heights("overture").is_err());
        assert!(!options.keep_interior_probes && options.corridor.is_none());
        let walked = parse(&[
            "--center".into(),
            "41.96,-87.67".into(),
            "--corridor".into(),
            "walk.gpx".into(),
            "--corridor-width-m".into(),
            "12".into(),
            "--keep-interior-probes".into(),
            "--output".into(),
            "unused".into(),
        ])
        .unwrap();
        assert_eq!(walked.corridor.as_deref(), Some(Path::new("walk.gpx")));
        assert_eq!(walked.corridor_width_m, 12.0);
        assert!(walked.keep_interior_probes);
        assert!(
            parse(&[
                "--center".into(),
                "41.96,-87.67".into(),
                "--corridor-width-m".into(),
                "12".into(),
                "--output".into(),
                "unused".into(),
            ])
            .is_err()
        );
        assert!((bake_planning_seconds(20_280, &options.bake) / 389.93 - 1.0).abs() < 0.1);
    }

    #[test]
    fn spawn_excludes_roof_footprints_in_both_windings() {
        let roof = [[0.0, 0.0], [2.0, 0.0], [0.0, 2.0]];
        assert!(point_in_triangle([0.5, 0.5], roof));
        assert!(point_in_triangle([0.5, 0.5], [roof[2], roof[1], roof[0]]));
        assert!(!point_in_triangle([1.5, 1.5], roof));
        assert!(!point_in_triangle([0.0, 0.0], [[0.0, 0.0]; 3]));
    }

    #[test]
    fn offline_bbox_request_matches_chicago_query() {
        let bbox = parse_bbox("41.87229,-87.62963,41.87453,-87.62919").unwrap();
        let (url, query) = overpass_request(bbox, false);
        assert_eq!(url, "https://overpass-api.de/api/interpreter");
        assert_eq!(
            query,
            include_str!("../../../fixtures/city/chicago-block/raw/query.overpassql").replace(
                ");\nout body;",
                &format!(
                    "  way[\"highway\"]({},{},{},{});\n);\nout body;",
                    bbox[0], bbox[1], bbox[2], bbox[3]
                )
            )
        );
        assert!(bbox_area(bbox) < MAX_AREA_M2);
        assert!(parse_bbox("0,0,1,1").is_ok());
        assert!(parse_bbox("1,0,0,1").is_err());
        assert!(parse_bbox("NaN,0,1,1").is_err());
    }

    #[test]
    fn street_spawn_and_corner_speaker_stay_outside_every_footprint() {
        let streets = vec![Street {
            id: "way/1".into(),
            name: "Street".into(),
            highway: "residential".into(),
            service: String::new(),
            points: vec![[-40.0, 0.0], [20.0, 0.0], [20.0, 40.0]],
        }];
        let footprints = [
            [[-4.0, -4.0], [4.0, -4.0], [4.0, 4.0]],
            [[-4.0, -4.0], [4.0, 4.0], [-4.0, 4.0]],
            [[19.0, 19.0], [21.0, 19.0], [20.0, 21.0]],
        ];
        let walkable = |p: [f32; 3]| {
            !footprints
                .iter()
                .any(|footprint| point_in_triangle([p[0], p[1]], *footprint))
        };
        let (spawn, source, forward, spots) =
            select_street_positions(&streets, 1.5, walkable, walkable).unwrap();
        assert!(walkable(spawn) && walkable(source));
        assert!(spawn[0].abs() < 6.0 && spawn[1] == 0.0);
        assert_eq!(forward, [1.0, 0.0, 0.0]);
        assert!((source[0] - spawn[0]).hypot(source[1] - spawn[1]) <= 30.0);
        // Spawn is west of the building on the street, which hides the
        // [20, 0] corner, so the speaker stands in the open street behind.
        assert!(spawn[0] < 0.0 && source[0] < spawn[0] - 6.0 && source[1] == 0.0);
        let scene = fixture(spawn, source, forward, &spots, &streets, &BakeConfig::default());
        let sources = scene["sources"].as_array().unwrap();
        assert_eq!(sources[0]["id"], "music");
        assert!(
            sources
                .iter()
                .all(|source| source["default_enabled"] == false)
        );
        assert!(scene.get("cues").is_none());
        assert_eq!(scene["simulation"]["reflections"]["enabled"], true);
    }

    #[test]
    fn spawn_skips_the_alley_and_the_speaker_stays_in_sight() {
        let street = |id: &str, highway: &str, points: Vec<[f32; 2]>| Street {
            id: id.into(),
            name: String::new(),
            highway: highway.into(),
            service: String::new(),
            points,
        };
        let streets = vec![
            street("way/1", "service", vec![[-20.0, 5.0], [20.0, 5.0]]),
            street("way/2", "residential", vec![[-40.0, -30.0], [40.0, -30.0]]),
            street("way/3", "residential", vec![[0.0, -30.0], [0.0, -60.0]]),
        ];
        // A house on the east side of the junction hides the corner from the
        // west; nothing hides the street ahead.
        let house = [[[3.0, -29.0], [12.0, -29.0], [12.0, -45.0]], [[3.0, -29.0], [12.0, -45.0], [3.0, -45.0]]];
        let open = |p: [f32; 3]| !house.iter().any(|tri| point_in_triangle([p[0], p[1]], *tri));
        let (spawn, source, _, _) = select_street_positions(&streets, 1.5, open, open).unwrap();
        assert_eq!(spawn[1], -30.0, "spawned in the alley at {spawn:?}");
        let steps = 64;
        assert!((1..steps).all(|step| {
            let t = step as f32 / steps as f32;
            open([spawn[0] + t * (source[0] - spawn[0]), spawn[1] + t * (source[1] - spawn[1]), 1.5])
        }), "speaker {source:?} is hidden from {spawn:?}");
    }

    #[test]
    fn relation_outer_ways_join_by_endpoints_without_duplicate_edges() {
        let points = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]].map(|p| json!(p));
        let rings = join_outer_rings(vec![
            vec![points[0].clone(), points[1].clone(), points[2].clone()],
            vec![points[0].clone(), points[3].clone(), points[2].clone()],
        ])
        .unwrap();
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 5);
        assert_eq!(rings[0].first(), rings[0].last());
        assert!(rings[0].windows(2).all(|pair| pair[0] != pair[1]));
    }
}
