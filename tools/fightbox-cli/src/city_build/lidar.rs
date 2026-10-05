//! Measured building heights from Cook County's 2022 airborne LiDAR.
//!
//! The Illinois State Geological Survey serves the county's 2022 surface
//! (DSM) and bare-earth (DTM) models as ArcGIS image services. One export of
//! each over the footprint bounds, on a WGS84 grid, gives every footprint the
//! median of DSM − DTM over the pixels inside it. The median, not a high
//! percentile, keeps street trees that overhang garages and small houses from
//! inflating them; it lands on the main roof of a flat-roofed two-flat and
//! mid-pitch on a gable. Over the first Ravenswood neighborhood it measured
//! two-level buildings at 8.6 m where `levels × 3.2` gives 6.4 m.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::city_place::{self, EARTH_RADIUS_M, Projection};
use crate::error::{CliError, Result};

const SERVICE_URL: &str = "https://data.isgs.illinois.edu/arcgis/rest/services/Elevation";
const SURFACE: &str = "IL_Cook_DSM_2022";
const BARE_EARTH: &str = "IL_Cook_DTM_2022";
pub(super) const ATTRIBUTION: &str =
    "Heights: Cook County 2022 LiDAR DSM/DTM, Illinois State Geological Survey";
/// Cook County as WGS84 south, west, north, east; the services hold NoData
/// outside it, so footprints there simply stay unmeasured.
const COVERAGE: [f64; 4] = [41.46, -88.27, 42.16, -87.52];
const US_SURVEY_FOOT_M: f64 = 1200.0 / 3937.0;
const NO_DATA: &str = "-9999";
const PIXEL_M: f64 = 0.5;
/// The services export at most 4100 rows.
const MAX_PIXELS: f64 = 4000.0;
const MARGIN_M: f64 = 5.0;
/// Edge pixels mix roof and ground; the inset is dropped only when it would
/// leave too few pixels (garages, narrow sheds).
const INSET_M: f64 = 0.5;
const MIN_PIXELS: usize = 8;
/// A lower median is a footprint LiDAR did not see standing in 2022 (newer
/// or demolished buildings, open canopies); OSM and the default keep those.
const MIN_HEIGHT_M: f64 = 2.0;
const MAX_HEIGHT_M: f64 = 450.0;

pub(super) struct Measured {
    pub heights: BTreeMap<String, f64>,
    pub metadata: Value,
    pub cache_hit: bool,
    /// The decoded rasters, for detail beyond footprints (fences, rail).
    pub rasters: Rasters,
}

/// Co-registered surface and bare-earth rasters, sampled by nearest pixel.
pub(super) struct Rasters {
    surface: Grid,
    bare: Grid,
}

impl Rasters {
    /// Height of the first return above bare earth, in metres.
    pub(super) fn above_ground_m(&self, longitude: f64, latitude: f64) -> Option<f64> {
        let index = self.surface.index(longitude, latitude)?;
        let value = f64::from(self.surface.values[index] - self.bare.values[index]);
        value.is_finite().then_some(value * US_SURVEY_FOOT_M)
    }

    /// A synthetic pair over `bounds` (south, west, north, east) at the
    /// production pixel size; `heights(lon, lat)` returns (surface, ground) m.
    #[cfg(test)]
    pub(super) fn synthetic(bounds: [f64; 4], heights: impl Fn(f64, f64) -> (f64, f64)) -> Self {
        let request = RasterRequest::around(bounds).expect("test bounds");
        let grid = |values| Grid {
            width: request.width,
            height: request.height,
            west: request.west,
            north: request.north,
            pixel_deg: [request.pixel_deg; 2],
            values,
        };
        let mut surface = grid(Vec::new());
        let mut bare = grid(Vec::new());
        for row in 0..request.height {
            for column in 0..request.width {
                let [lon, lat] = surface.centre(column, row);
                let (top, ground) = heights(lon, lat);
                surface.values.push((top / US_SURVEY_FOOT_M) as f32);
                bare.values.push((ground / US_SURVEY_FOOT_M) as f32);
            }
        }
        Self { surface, bare }
    }

    /// Bare-earth elevation, in metres.
    pub(super) fn ground_m(&self, longitude: f64, latitude: f64) -> Option<f64> {
        let value = f64::from(self.bare.values[self.bare.index(longitude, latitude)?]);
        value.is_finite().then_some(value * US_SURVEY_FOOT_M)
    }
}

pub(super) fn covers([south, west, north, east]: [f64; 4]) -> bool {
    south >= COVERAGE[0] && west >= COVERAGE[1] && north <= COVERAGE[2] && east <= COVERAGE[3]
}

/// Fetches both rasters once per footprint area (cached beside the Overpass
/// response) and measures every converted footprint. Writes the per-building
/// table to `lidar_heights.json`.
pub(super) fn measure(output: &Path, geojson: &Value, footprints: [f64; 4]) -> Result<Measured> {
    let request = RasterRequest::around(footprints)?;
    let mut cache_hit = true;
    let mut fetch = |name: &str, service: &str| -> Result<(Grid, Value)> {
        let url = format!("{SERVICE_URL}/{service}/ImageServer/exportImage");
        let parameters = request.parameters();
        let parameters = parameters
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect::<Vec<_>>();
        let response = city_place::fetch_cached_tiff(output, name, &url, &parameters)?;
        cache_hit &= response.cache_hit;
        Ok((decode(&response.raw, &request, name)?, response.metadata))
    };
    let (surface, surface_metadata) = fetch("lidar_dsm", SURFACE)?;
    let (bare, bare_metadata) = fetch("lidar_dtm", BARE_EARTH)?;
    if surface.width != bare.width
        || surface.height != bare.height
        || surface.west != bare.west
        || surface.north != bare.north
        || surface.pixel_deg != bare.pixel_deg
    {
        return Err(CliError::new(
            "LiDAR surface and bare-earth rasters are not on the same grid",
        ));
    }
    let rows = measure_footprints(geojson, &surface, &bare)?;
    let heights = rows
        .iter()
        .filter_map(|row| row.height_m.map(|height| (row.id.clone(), height)))
        .collect::<BTreeMap<_, _>>();
    let mut status = BTreeMap::<&str, usize>::new();
    for row in &rows {
        *status.entry(row.status).or_default() += 1;
    }
    let metadata = json!({
        "source": ATTRIBUTION, "vintage": "flown 2022-04 to 2022-06",
        "statistic": "median of DSM - DTM over pixels inside the footprint, inset 0.5 m; US survey feet to metres",
        "raster_size": [request.width, request.height], "pixel_deg": request.pixel_deg,
        "raster_west_south_east_north": [request.west, request.south(), request.east(), request.north],
        "footprint_status": status, "measured_count": heights.len(),
        "dsm": surface_metadata, "dtm": bare_metadata,
    });
    crate::atomicio::write_json_atomic(
        &output.join("lidar_heights.json"),
        &json!({"source": ATTRIBUTION, "buildings": rows.iter().map(|row| json!({
            "id": row.id, "height_m": row.height_m, "median_m": row.median_m,
            "pixels": row.pixels, "status": row.status})).collect::<Vec<_>>()}),
    )?;
    Ok(Measured {
        heights,
        metadata,
        cache_hit,
        rasters: Rasters { surface, bare },
    })
}

/// A north-up WGS84 export whose pixels are square in degrees and
/// `PIXEL_M` tall, so pixel centres map linearly onto longitude/latitude.
struct RasterRequest {
    west: f64,
    north: f64,
    pixel_deg: f64,
    width: usize,
    height: usize,
}

impl RasterRequest {
    fn around([south, west, north, east]: [f64; 4]) -> Result<Self> {
        let metres_per_degree = EARTH_RADIUS_M.to_radians();
        let margin_lat = MARGIN_M / metres_per_degree;
        let margin_lon = margin_lat / ((south + north) * 0.5).to_radians().cos();
        let (south, west) = (south - margin_lat, west - margin_lon);
        let (north, east) = (north + margin_lat, east + margin_lon);
        let pixel_deg = (PIXEL_M / metres_per_degree)
            .max((north - south) / MAX_PIXELS)
            .max((east - west) / MAX_PIXELS);
        let width = ((east - west) / pixel_deg).ceil();
        let height = ((north - south) / pixel_deg).ceil();
        if !(width.is_finite() && height.is_finite() && width >= 1.0 && height >= 1.0) {
            return Err(CliError::new("LiDAR raster bounds are empty"));
        }
        Ok(Self {
            west,
            north,
            pixel_deg,
            width: width as usize,
            height: height as usize,
        })
    }

    fn east(&self) -> f64 {
        self.west + self.width as f64 * self.pixel_deg
    }

    fn south(&self) -> f64 {
        self.north - self.height as f64 * self.pixel_deg
    }

    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "bbox",
                format!(
                    "{},{},{},{}",
                    self.west,
                    self.south(),
                    self.east(),
                    self.north
                ),
            ),
            ("bboxSR", "4326".into()),
            ("imageSR", "4326".into()),
            ("size", format!("{},{}", self.width, self.height)),
            ("format", "tiff".into()),
            ("pixelType", "F32".into()),
            ("noData", NO_DATA.into()),
            ("interpolation", "RSP_NearestNeighbor".into()),
            ("f", "image".into()),
        ]
    }
}

struct Grid {
    width: usize,
    height: usize,
    west: f64,
    north: f64,
    pixel_deg: [f64; 2],
    /// Elevation in US survey feet, row-major from the north-west corner;
    /// NaN where the service has no data.
    values: Vec<f32>,
}

impl Grid {
    fn index(&self, longitude: f64, latitude: f64) -> Option<usize> {
        let column = (longitude - self.west) / self.pixel_deg[0];
        let row = (self.north - latitude) / self.pixel_deg[1];
        (column >= 0.0
            && row >= 0.0
            && (column as usize) < self.width
            && (row as usize) < self.height)
            .then(|| row as usize * self.width + column as usize)
    }

    fn centre(&self, column: usize, row: usize) -> [f64; 2] {
        [
            self.west + (column as f64 + 0.5) * self.pixel_deg[0],
            self.north - (row as f64 + 0.5) * self.pixel_deg[1],
        ]
    }
}

fn decode(bytes: &[u8], request: &RasterRequest, name: &str) -> Result<Grid> {
    let invalid = |error: tiff::TiffError| CliError::new(format!("{name} GeoTIFF: {error}"));
    let mut decoder = Decoder::new(std::io::Cursor::new(bytes)).map_err(invalid)?;
    let (width, height) = decoder.dimensions().map_err(invalid)?;
    let scale = decoder
        .get_tag_f64_vec(Tag::ModelPixelScaleTag)
        .map_err(invalid)?;
    let tiepoint = decoder
        .get_tag_f64_vec(Tag::ModelTiepointTag)
        .map_err(invalid)?;
    if scale.len() < 2 || tiepoint.len() < 6 {
        return Err(CliError::new(format!(
            "{name} GeoTIFF lacks georeferencing"
        )));
    }
    let pixel_deg = [scale[0], scale[1]];
    let west = tiepoint[3] - tiepoint[0] * pixel_deg[0];
    let north = tiepoint[4] + tiepoint[1] * pixel_deg[1];
    // A projected export (metres or feet) or a shifted extent fails here
    // instead of silently measuring the wrong pixels.
    let tolerance = 2.0 * request.pixel_deg;
    if (width as usize, height as usize) != (request.width, request.height)
        || pixel_deg
            .iter()
            .any(|value| (value - request.pixel_deg).abs() > 0.05 * request.pixel_deg)
        || (west - request.west).abs() > tolerance
        || (north - request.north).abs() > tolerance
    {
        return Err(CliError::new(format!(
            "{name} GeoTIFF grid {width}x{height} at {west},{north} (pixel {pixel_deg:?}) does not match the requested WGS84 grid"
        )));
    }
    let values = match decoder.read_image().map_err(invalid)? {
        DecodingResult::F32(values) => values,
        _ => {
            return Err(CliError::new(format!("{name} GeoTIFF is not 32-bit float")));
        }
    };
    let values = values
        .into_iter()
        .map(|value| {
            if value.is_finite() && value > -1000.0 {
                value
            } else {
                f32::NAN
            }
        })
        .collect::<Vec<_>>();
    if values.len() != request.width * request.height {
        return Err(CliError::new(format!(
            "{name} GeoTIFF has the wrong pixel count"
        )));
    }
    Ok(Grid {
        width: request.width,
        height: request.height,
        west,
        north,
        pixel_deg,
        values,
    })
}

struct Row {
    id: String,
    median_m: Option<f64>,
    height_m: Option<f64>,
    pixels: usize,
    status: &'static str,
}

fn measure_footprints(geojson: &Value, surface: &Grid, bare: &Grid) -> Result<Vec<Row>> {
    let frame = Projection {
        center: [
            surface.north - surface.height as f64 * surface.pixel_deg[1] * 0.5,
            surface.west + surface.width as f64 * surface.pixel_deg[0] * 0.5,
        ],
    };
    let features = geojson["features"]
        .as_array()
        .ok_or_else(|| CliError::new("converted GeoJSON needs features"))?;
    let mut rows = Vec::with_capacity(features.len());
    for feature in features {
        let id = feature["id"].as_str().unwrap_or_default().to_owned();
        let ring = feature["geometry"]["coordinates"][0]
            .as_array()
            .ok_or_else(|| CliError::new(format!("{id}: footprint needs an outer ring")))?
            .iter()
            .map(|point| {
                let lon = point[0].as_f64().unwrap_or(f64::NAN);
                let lat = point[1].as_f64().unwrap_or(f64::NAN);
                ([lon, lat], frame.project(lon, lat))
            })
            .collect::<Vec<_>>();
        let local = ring.iter().map(|(_, xy)| *xy).collect::<Vec<_>>();
        let (lon_min, lon_max, lat_min, lat_max) = ring.iter().fold(
            (
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
            ),
            |(a, b, c, d), ([lon, lat], _)| (a.min(*lon), b.max(*lon), c.min(*lat), d.max(*lat)),
        );
        let column = |lon: f64| (lon - surface.west) / surface.pixel_deg[0];
        let row = |lat: f64| (surface.north - lat) / surface.pixel_deg[1];
        let columns = column(lon_min).floor().max(0.0) as usize
            ..(column(lon_max).ceil().max(0.0) as usize).min(surface.width);
        let rows_range = row(lat_max).floor().max(0.0) as usize
            ..(row(lat_min).ceil().max(0.0) as usize).min(surface.height);
        let mut inset = Vec::new();
        let mut all = Vec::new();
        for r in rows_range {
            for c in columns.clone() {
                let [lon, lat] = surface.centre(c, r);
                let point = frame.project(lon, lat);
                if !point_in_ring(point, &local) {
                    continue;
                }
                let index = r * surface.width + c;
                let value = f64::from(surface.values[index] - bare.values[index]);
                if !value.is_finite() {
                    continue;
                }
                let metres = value * US_SURVEY_FOOT_M;
                all.push(metres);
                if edge_distance(point, &local) >= INSET_M {
                    inset.push(metres);
                }
            }
        }
        let mut samples = if inset.len() >= MIN_PIXELS {
            inset
        } else {
            all
        };
        let pixels = samples.len();
        let median_m = (pixels >= MIN_PIXELS).then(|| median(&mut samples));
        let (height_m, status) = match median_m {
            None => (None, "too_few_pixels"),
            Some(value) if value < MIN_HEIGHT_M => (None, "below_2m"),
            Some(value) if value > MAX_HEIGHT_M => (None, "above_450m"),
            Some(value) => (Some((value * 10.0).round() / 10.0), "measured"),
        };
        rows.push(Row {
            id,
            median_m,
            height_m,
            pixels,
            status,
        });
    }
    Ok(rows)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    }
}

fn point_in_ring([x, y]: [f64; 2], ring: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for pair in ring.windows(2) {
        let ([x1, y1], [x2, y2]) = (pair[0], pair[1]);
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    inside
}

fn edge_distance([x, y]: [f64; 2], ring: &[[f64; 2]]) -> f64 {
    ring.windows(2)
        .map(|pair| {
            let ([ax, ay], [bx, by]) = (pair[0], pair[1]);
            let (dx, dy) = (bx - ax, by - ay);
            let length = dx * dx + dy * dy;
            let t = if length > 0.0 {
                (((x - ax) * dx + (y - ay) * dy) / length).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (ax + t * dx - x).hypot(ay + t * dy - y)
        })
        .fold(f64::INFINITY, f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 40 x 40 pixel grid with ground at 600 ft, a 10 m roof over a
    /// footprint, and an 8 m tree crown spilling over one corner.
    #[test]
    fn median_height_ignores_overhanging_canopy_and_edges() {
        let request = RasterRequest::around([41.965, -87.673, 41.9652, -87.6727]).unwrap();
        let (width, height) = (request.width, request.height);
        let ground = || Grid {
            width,
            height,
            west: request.west,
            north: request.north,
            pixel_deg: [request.pixel_deg; 2],
            values: vec![600.0; width * height],
        };
        let (mut surface, bare) = (ground(), ground());
        let roof_ft = (10.0 / US_SURVEY_FOOT_M) as f32;
        let tree_ft = (18.0 / US_SURVEY_FOOT_M) as f32;
        let footprint = [
            [-87.67295, 41.96505],
            [-87.67275, 41.96505],
            [-87.67275, 41.96515],
            [-87.67295, 41.96515],
            [-87.67295, 41.96505],
        ];
        let frame = Projection {
            center: [41.9651, -87.67285],
        };
        let local = footprint.map(|[lon, lat]| frame.project(lon, lat));
        for row in 0..height {
            for column in 0..width {
                let [lon, lat] = surface.centre(column, row);
                let point = frame.project(lon, lat);
                let index = row * width + column;
                if point_in_ring(point, &local) {
                    surface.values[index] += roof_ft;
                    if point[0] > 5.0 && point[1] > 3.0 {
                        surface.values[index] = 600.0 + tree_ft;
                    }
                }
            }
        }
        surface.values[0] = f32::NAN;
        let geojson = json!({"features": [
            {"id": "way/1", "geometry": {"coordinates": [footprint.to_vec()]}},
            {"id": "way/2", "geometry": {"coordinates": [[[-87.6729, 41.96501], [-87.67289, 41.96501], [-87.67289, 41.965015], [-87.6729, 41.96501]]]}},
        ]});
        let rows = measure_footprints(&geojson, &surface, &bare).unwrap();
        assert_eq!(rows[0].status, "measured");
        assert!(
            (rows[0].height_m.unwrap() - 10.0).abs() < 0.05,
            "{:?}",
            rows[0].median_m
        );
        assert!(rows[0].pixels > 100);
        assert_eq!(rows[1].status, "too_few_pixels");
        assert!(rows[1].height_m.is_none());
    }

    #[test]
    fn request_grid_is_square_in_degrees_and_bounded() {
        let request = RasterRequest::around([41.9634, -87.6760, 41.9679, -87.6700]).unwrap();
        assert!((request.pixel_deg * EARTH_RADIUS_M.to_radians() - PIXEL_M).abs() < 1e-9);
        assert!(request.east() >= -87.6700 && request.south() <= 41.9634);
        assert!(request.width < 4100 && request.height < 4100);
        let wide = RasterRequest::around([41.0, -88.0, 41.5, -87.0]).unwrap();
        assert!(wide.width as f64 <= MAX_PIXELS + 1.0 && wide.height as f64 <= MAX_PIXELS + 1.0);
        assert!(covers([41.9634, -87.6760, 41.9679, -87.6700]));
        assert!(!covers([40.0, -89.0, 40.1, -88.9]));
    }
}
