//! Geographic frame and cached, user-triggered OSM requests for city builds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::atomicio::{write_bytes_atomic, write_json_atomic};
use crate::error::{CliError, Result};

pub(crate) const EARTH_RADIUS_M: f64 = 6_371_008.8;
pub(crate) const ATTRIBUTION: &str = "© OpenStreetMap contributors, ODbL";
const USER_AGENT: &str =
    "fightbox/0.1 (local acoustic city builder; https://github.com/mduncs/spatial-audio-engine)";

#[derive(Clone, Copy)]
pub(crate) struct Projection {
    pub center: [f64; 2], // latitude, longitude
}

impl Projection {
    pub fn project(&self, longitude: f64, latitude: f64) -> [f64; 2] {
        [
            (longitude - self.center[1]).to_radians()
                * EARTH_RADIUS_M
                * self.center[0].to_radians().cos(),
            (latitude - self.center[0]).to_radians() * EARTH_RADIUS_M,
        ]
    }

    pub fn unproject(&self, east_m: f64, north_m: f64) -> [f64; 2] {
        [
            self.center[0] + (north_m / EARTH_RADIUS_M).to_degrees(),
            self.center[1]
                + (east_m / (EARTH_RADIUS_M * self.center[0].to_radians().cos())).to_degrees(),
        ]
    }

    pub fn metadata(&self) -> Value {
        json!({
            "origin": {"latitude_degrees": self.center[0], "longitude_degrees": self.center[1], "altitude_m": 0.0},
            "projection": {"name": "local_equirectangular", "datum": "WGS84",
                "earth_radius_m": EARTH_RADIUS_M, "axes": "x east, y north, z up",
                "formula": "x=R*cos(lat0)*(lon-lon0); y=R*(lat-lat0), angles in radians"},
        })
    }
}

pub(crate) fn parse_center(value: &str) -> Result<[f64; 2]> {
    let values = value
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| CliError::new("--center requires latitude,longitude in degrees"))?;
    let center: [f64; 2] = values
        .try_into()
        .map_err(|_| CliError::new("--center requires two coordinates"))?;
    validate_center(center)?;
    Ok(center)
}

pub(crate) fn project_geojson(input: &[u8], projection: Projection) -> Result<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(input)
        .map_err(|error| CliError::new(format!("invalid GeoJSON: {error}")))?;
    let features = root["features"]
        .as_array_mut()
        .ok_or_else(|| CliError::new("GeoJSON features must be an array"))?;
    for feature in features {
        let rings = feature["geometry"]["coordinates"]
            .as_array_mut()
            .ok_or_else(|| CliError::new("GeoJSON polygon coordinates must be arrays"))?;
        for ring in rings {
            let points = ring
                .as_array_mut()
                .ok_or_else(|| CliError::new("invalid GeoJSON ring"))?;
            for point in points {
                let coordinates = point
                    .as_array_mut()
                    .ok_or_else(|| CliError::new("invalid GeoJSON point"))?;
                let longitude = coordinates
                    .first()
                    .and_then(Value::as_f64)
                    .ok_or_else(|| CliError::new("invalid GeoJSON longitude"))?;
                let latitude = coordinates
                    .get(1)
                    .and_then(Value::as_f64)
                    .ok_or_else(|| CliError::new("invalid GeoJSON latitude"))?;
                validate_center([latitude, longitude])?;
                let [east_m, north_m] = projection.project(longitude, latitude);
                coordinates[0] = json!(east_m);
                coordinates[1] = json!(north_m);
            }
        }
    }
    serde_json::to_vec(&root).map_err(|error| CliError::new(error.to_string()))
}

fn validate_center([latitude, longitude]: [f64; 2]) -> Result<()> {
    if !latitude.is_finite()
        || !longitude.is_finite()
        || latitude.abs() >= 90.0
        || !(-180.0..=180.0).contains(&longitude)
    {
        return Err(CliError::new(
            "place center needs finite WGS84 latitude/longitude away from the poles",
        ));
    }
    Ok(())
}

pub(crate) fn bbox_from_center(center: [f64; 2], radius_m: f64) -> Result<[f64; 4]> {
    validate_center(center)?;
    if !radius_m.is_finite() || radius_m <= 0.0 {
        return Err(CliError::new(
            "--radius-m requires a finite positive number",
        ));
    }
    let frame = Projection { center };
    let [south, west] = frame.unproject(-radius_m, -radius_m);
    let [north, east] = frame.unproject(radius_m, radius_m);
    if south <= -90.0 || north >= 90.0 || west < -180.0 || east > 180.0 {
        return Err(CliError::new(
            "radius crosses a pole or antimeridian; choose a smaller area",
        ));
    }
    Ok([south, west, north, east])
}

pub(crate) fn parse_nominatim(raw: &[u8]) -> Result<([f64; 2], String)> {
    let data: Value = serde_json::from_slice(raw)
        .map_err(|error| CliError::new(format!("invalid Nominatim JSON: {error}")))?;
    let places = data
        .as_array()
        .ok_or_else(|| CliError::new("Nominatim response needs an array"))?;
    let place = places.first().ok_or_else(|| {
        CliError::new("Nominatim found no place; use --center latitude,longitude for this location")
    })?;
    let coordinate = |key: &str| -> Result<f64> {
        place[key]
            .as_str()
            .and_then(|raw| raw.parse::<f64>().ok())
            .ok_or_else(|| CliError::new(format!("Nominatim result needs numeric {key}")))
    };
    let center = [coordinate("lat")?, coordinate("lon")?];
    validate_center(center)?;
    Ok((
        center,
        place["display_name"]
            .as_str()
            .unwrap_or("OSM place")
            .to_owned(),
    ))
}

pub(crate) struct Intersection<'a> {
    pub first: &'a str,
    pub second: &'a str,
    pub locality: &'a str,
}

pub(crate) fn intersection(place: &str) -> Option<Intersection<'_>> {
    let (streets, locality) = place.split_once(',')?;
    let lower = streets.to_ascii_lowercase();
    let (index, width) = lower
        .find(" and ")
        .map(|index| (index, 5))
        .or_else(|| lower.find('&').map(|index| (index, 1)))?;
    let first = streets[..index].trim();
    let second = streets[index + width..].trim();
    if first.is_empty() || second.is_empty() || locality.trim().is_empty() {
        return None;
    }
    Some(Intersection {
        first,
        second,
        locality: locality.trim(),
    })
}

pub(crate) fn intersection_query(raw: &[u8], intersection: &Intersection<'_>) -> Result<String> {
    let data: Value =
        serde_json::from_slice(raw).map_err(|error| CliError::new(error.to_string()))?;
    let bounds = data[0]["boundingbox"]
        .as_array()
        .filter(|bounds| bounds.len() == 4)
        .ok_or_else(|| {
            CliError::new("Nominatim locality needs boundingbox to resolve an intersection")
        })?;
    let bounds = bounds
        .iter()
        .map(|value| value.as_str().and_then(|s| s.parse::<f64>().ok()))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CliError::new("invalid locality boundingbox"))?;
    validate_center([bounds[0], bounds[2]])?;
    validate_center([bounds[1], bounds[3]])?;
    if bounds[0] >= bounds[1] || bounds[2] >= bounds[3] {
        return Err(CliError::new("locality bounds must increase"));
    }
    let bounds = format!("{},{},{},{}", bounds[0], bounds[2], bounds[1], bounds[3]);
    let regex = |street: &str| {
        let escaped = street
            .chars()
            .flat_map(|c| {
                if ".*+?()[]{}^$|\\".contains(c) {
                    vec!['\\', c]
                } else {
                    vec![c]
                }
            })
            .collect::<String>();
        serde_json::to_string(&format!("(^| ){escaped}( |$)")).expect("string serialization")
    };
    Ok(format!(
        "[out:json][timeout:90];\n(\n  way[\"highway\"][\"name\"~{},i]({bounds});\n  way[\"highway\"][\"name\"~{},i]({bounds});\n);\nout body;\n>;\nout skel qt;\n",
        regex(intersection.first),
        regex(intersection.second)
    ))
}

pub(crate) fn resolve_intersection(
    raw: &[u8],
    intersection: &Intersection<'_>,
    near: [f64; 2],
) -> Result<[f64; 2]> {
    let data: Value =
        serde_json::from_slice(raw).map_err(|error| CliError::new(error.to_string()))?;
    let elements = data["elements"]
        .as_array()
        .ok_or_else(|| CliError::new("street lookup needs elements"))?;
    let mut first = BTreeSet::new();
    let mut second = BTreeSet::new();
    let mut nodes = BTreeMap::new();
    for element in elements {
        if element["type"] == "node" {
            if let (Some(id), Some(lat), Some(lon)) = (
                element["id"].as_i64(),
                element["lat"].as_f64(),
                element["lon"].as_f64(),
            ) {
                validate_center([lat, lon])?;
                nodes.insert(id, [lat, lon]);
            }
        } else if element["type"] == "way" {
            let name = element["tags"]["name"]
                .as_str()
                .unwrap_or("")
                .to_ascii_lowercase();
            let references = element["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64);
            if name.contains(&intersection.first.to_ascii_lowercase()) {
                first.extend(references.clone());
            }
            if name.contains(&intersection.second.to_ascii_lowercase()) {
                second.extend(references);
            }
        }
    }
    let frame = Projection { center: near };
    first
        .intersection(&second)
        .filter_map(|id| nodes.get(id))
        .min_by(|a, b| {
            let distance = |center: &&[f64; 2]| {
                let [x, y] = frame.project(center[1], center[0]);
                x.hypot(y)
            };
            distance(a).total_cmp(&distance(b))
        })
        .copied()
        .ok_or_else(|| {
            CliError::new(
                "named streets have no shared OSM intersection; use --center latitude,longitude",
            )
        })
}

pub(crate) struct Response {
    pub raw: Vec<u8>,
    pub metadata: Value,
    pub cache_hit: bool,
}

/// Request identity and content hash prevent reusing a different area after a
/// user changes the command. Successful raw responses also survive a failed bake.
pub(crate) fn fetch_cached(
    output: &Path,
    name: &str,
    url: &str,
    parameters: &[(&str, &str)],
) -> Result<Response> {
    fetch(output, name, url, parameters, Payload::Json)
}

/// The same cache for a binary GeoTIFF export (image services answer errors
/// with a JSON or HTML body, so the TIFF signature is required).
pub(crate) fn fetch_cached_tiff(
    output: &Path,
    name: &str,
    url: &str,
    parameters: &[(&str, &str)],
) -> Result<Response> {
    fetch(output, name, url, parameters, Payload::Tiff)
}

#[derive(Clone, Copy, PartialEq)]
enum Payload {
    Json,
    Tiff,
}

fn fetch(
    output: &Path,
    name: &str,
    url: &str,
    parameters: &[(&str, &str)],
    payload: Payload,
) -> Result<Response> {
    let extension = match payload {
        Payload::Json => "json",
        Payload::Tiff => "tif",
    };
    let raw_path = output.join(format!("{name}_raw.{extension}"));
    let metadata_path = output.join(format!("{name}_request.json"));
    let request = json!({"url": url, "parameters": parameters});
    if let (Ok(raw), Ok(metadata)) = (std::fs::read(&raw_path), std::fs::read(&metadata_path)) {
        if let Ok(metadata) = serde_json::from_slice::<Value>(&metadata) {
            if metadata["request"] == request
                && metadata["response_sha256"] == fightbox_evidence::sha256_hex(&raw)
            {
                eprintln!("fightbox: {name} response cache hit");
                return Ok(Response {
                    raw,
                    metadata,
                    cache_hit: true,
                });
            }
        }
    }
    let _rate_lock = if name == "nominatim" {
        Some(nominatim_rate_lock()?)
    } else {
        None
    };
    eprintln!("fightbox: fetching {name} from {url}");
    let mut command = std::process::Command::new("curl");
    command.args([
        "--fail",
        "--silent",
        "--show-error",
        "--max-time",
        "110",
        "--user-agent",
        USER_AGENT,
        "--get",
    ]);
    if name != "nominatim" {
        command.args(["--retry", "2", "--retry-delay", "2"]);
    }
    for (key, value) in parameters {
        command.args(["--data-urlencode", &format!("{key}={value}")]);
    }
    let response = command
        .arg(url)
        .output()
        .map_err(|error| CliError::new(format!("cannot run curl for {name}: {error}")))?;
    if !response.status.success() {
        return Err(CliError::new(format!(
            "{name} fetch failed: {}",
            String::from_utf8_lossy(&response.stderr)
        )));
    }
    let data_timestamp = match payload {
        Payload::Json => {
            let data: Value = serde_json::from_slice(&response.stdout)
                .map_err(|error| CliError::new(format!("invalid {name} response: {error}")))?;
            if let Some(remark) = data.get("remark").and_then(Value::as_str) {
                return Err(CliError::new(format!(
                    "{name} returned incomplete data: {remark}"
                )));
            }
            data.pointer("/osm3s/timestamp_osm_base").cloned()
        }
        Payload::Tiff => {
            if !(response.stdout.starts_with(b"II*\0") || response.stdout.starts_with(b"MM\0*")) {
                let body = String::from_utf8_lossy(&response.stdout);
                return Err(CliError::new(format!(
                    "{name} did not return a GeoTIFF: {}",
                    body.chars().take(300).collect::<String>()
                )));
            }
            None
        }
    };
    let metadata = json!({"request": request, "fetched_at_unix_s": unix_time(),
        "data_timestamp": data_timestamp,
        "response_sha256": fightbox_evidence::sha256_hex(&response.stdout)});
    write_bytes_atomic(&raw_path, &response.stdout)?;
    write_json_atomic(&metadata_path, &metadata)?;
    Ok(Response {
        raw: response.stdout,
        metadata,
        cache_hit: false,
    })
}

fn unix_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn nominatim_rate_lock() -> Result<std::fs::File> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    let path = std::env::temp_dir().join(format!("fightbox-nominatim-{}.lock", unsafe {
        libc::geteuid()
    }));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| CliError::new(format!("cannot open geocoding rate lock: {error}")))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(CliError::new("cannot lock geocoding rate limit"));
    }
    let mut last = String::new();
    file.read_to_string(&mut last)
        .map_err(|error| CliError::new(error.to_string()))?;
    if let Ok(last) = last.parse::<f64>() {
        let remaining = (1.0 - (unix_time() - last)).clamp(0.0, 1.0);
        if remaining > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(remaining));
        }
    }
    file.set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)))
        .and_then(|_| write!(file, "{}", unix_time()))
        .map_err(|error| CliError::new(error.to_string()))?;
    Ok(file)
}

pub(crate) fn slug(place: &str) -> String {
    let slug = place
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "city".to_owned()
    } else {
        slug.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominatim_response_parsing() {
        let (center, name) = parse_nominatim(
            br#"[{"lat":"41.8819","lon":"-87.6278","display_name":"State and Madison"}]"#,
        )
        .unwrap();
        assert_eq!(center, [41.8819, -87.6278]);
        assert_eq!(name, "State and Madison");
        assert!(parse_nominatim(b"[]").is_err());
        assert!(parse_nominatim(br#"[{"lat":"NaN","lon":"0"}]"#).is_err());
        assert!(parse_nominatim(br#"[{"lat":"91","lon":"0"}]"#).is_err());
        let intersection = intersection("State and Madison, Chicago").unwrap();
        assert_eq!(intersection.locality, "Chicago");
        let streets =
            br#"{"elements":[{"type":"way","tags":{"name":"North State Street"},"nodes":[1,2]},
            {"type":"way","tags":{"name":"West Madison Street"},"nodes":[2,3]},
            {"type":"node","id":2,"lat":41.88,"lon":-87.62}]}"#;
        assert_eq!(
            resolve_intersection(streets, &intersection, center).unwrap(),
            [41.88, -87.62]
        );
    }

    #[test]
    fn center_radius_bbox_is_a_square_in_enu() {
        let center = parse_center("41.8819,-87.6278").unwrap();
        let bbox = bbox_from_center(center, 250.0).unwrap();
        let frame = Projection { center };
        for (got, expected) in frame
            .project(bbox[1], bbox[0])
            .into_iter()
            .zip([-250.0, -250.0])
        {
            assert!((got - expected).abs() < 1e-6);
        }
        assert!(bbox_from_center(center, 0.0).is_err());
        assert!(bbox_from_center([0.0, 179.999], 250.0).is_err());
    }

    #[test]
    fn origin_projection_round_trip() {
        let frame = Projection {
            center: [41.8819, -87.6278],
        };
        assert_eq!(frame.project(frame.center[1], frame.center[0]), [0.0, 0.0]);
        for enu in [[250.0, -150.0], [-250.0, 250.0], [0.0, 0.0]] {
            let [latitude, longitude] = frame.unproject(enu[0], enu[1]);
            for (got, expected) in frame.project(longitude, latitude).into_iter().zip(enu) {
                assert!((got - expected).abs() < 1e-6);
            }
        }
    }
}
