//! Facade, roof, and garage materials from the Cook County Assessor.
//!
//! Residential parcels (class 2xx) carry the assessor's exterior wall,
//! roof construction, and detached-garage construction. Each footprint takes
//! the parcel whose centroid it contains, else the nearest within
//! `MATCH_RADIUS_M`; a small or garage-tagged footprint is that parcel's
//! garage. A large footprint without a residential parcel (apartment blocks,
//! commercial rows, condominiums) is Chicago masonry with a flat roof; smaller
//! unmatched footprints and OSM-tagged materials are left alone.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::city_place::{self, Projection};
use crate::error::{CliError, Result};

pub(super) const ATTRIBUTION: &str = "Cook County Assessor's Office, Cook County Open Data";
const PARCELS_URL: &str = "https://datacatalog.cookcountyil.gov/resource/pabr-t5kh.json";
const CHARACTERISTICS_URL: &str = "https://datacatalog.cookcountyil.gov/resource/x54s-btds.json";
const PIN_CHUNK: usize = 120;
const MATCH_RADIUS_M: f64 = 25.0;
/// A two-car Chicago garage is ~40 m²; the smallest houses exceed 70 m².
const GARAGE_MAX_AREA_M2: f64 = 70.0;
/// Unassessed footprints this large are multi-unit or commercial masonry.
const UNASSESSED_MASONRY_MIN_AREA_M2: f64 = 300.0;

#[derive(Debug, Default)]
struct Characteristics {
    wall: String,
    roof: String,
    garage: String,
}

pub(super) struct Assessed {
    pub metadata: Value,
    pub cache_hit: bool,
}

/// Sets `material`/`roof_material` on residential footprints in place.
pub(super) fn apply(output: &Path, geojson: &mut Value, footprints: [f64; 4]) -> Result<Assessed> {
    let [south, west, north, east] = footprints;
    let mut cache_hit = true;
    let parcels = city_place::fetch_cached(
        output,
        "assessor_parcels",
        PARCELS_URL,
        &[
            ("$select", "pin,class,lon,lat"),
            (
                "$where",
                &format!("lat between {south} and {north} and lon between {west} and {east}"),
            ),
            ("$limit", "50000"),
        ],
    )?;
    cache_hit &= parcels.cache_hit;
    let parcels = parse_parcels(&parcels.raw)?;
    let pins = parcels.keys().cloned().collect::<Vec<_>>();
    let mut rows = Vec::new();
    let mut requests = Vec::new();
    for (index, chunk) in pins.chunks(PIN_CHUNK).enumerate() {
        let list = chunk
            .iter()
            .map(|pin| format!("'{pin}'"))
            .collect::<Vec<_>>()
            .join(",");
        let response = city_place::fetch_cached(
            output,
            &format!("assessor_characteristics_{index}"),
            CHARACTERISTICS_URL,
            &[
                (
                    "$select",
                    "pin,year,card,char_ext_wall,char_roof_cnst,char_gar1_cnst",
                ),
                ("$where", &format!("year >= '2024' and pin in ({list})")),
                ("$limit", "50000"),
            ],
        )?;
        cache_hit &= response.cache_hit;
        requests.push(response.metadata);
        rows.extend(
            serde_json::from_slice::<Vec<Value>>(&response.raw).map_err(|error| {
                CliError::new(format!("invalid assessor characteristics: {error}"))
            })?,
        );
    }
    let characteristics = latest_characteristics(&rows);
    let located = parcels
        .iter()
        .filter_map(|(pin, location)| Some((*location, characteristics.get(pin)?)))
        .collect::<Vec<_>>();
    let counts = assign(geojson, &located);
    Ok(Assessed {
        metadata: json!({"source": ATTRIBUTION, "residential_parcels": located.len(),
            "parcels": parcels.len(), "materials": counts, "characteristics_requests": requests.len()}),
        cache_hit,
    })
}

/// Residential (class 2xx, not 299 condominium) parcel centroids by PIN.
fn parse_parcels(raw: &[u8]) -> Result<BTreeMap<String, [f64; 2]>> {
    let rows: Vec<Value> = serde_json::from_slice(raw)
        .map_err(|error| CliError::new(format!("invalid assessor parcels: {error}")))?;
    Ok(rows
        .iter()
        .filter(|row| {
            row["class"]
                .as_str()
                .is_some_and(|class| class.starts_with('2') && class != "299")
        })
        .filter_map(|row| {
            let number = |key: &str| row[key].as_str()?.parse::<f64>().ok();
            Some((
                row["pin"].as_str()?.to_owned(),
                [number("lon")?, number("lat")?],
            ))
        })
        .collect())
}

/// The newest year's first card per PIN.
fn latest_characteristics(rows: &[Value]) -> BTreeMap<String, Characteristics> {
    let mut best = BTreeMap::<String, ((i64, i64), &Value)>::new();
    for row in rows {
        let Some(pin) = row["pin"].as_str() else {
            continue;
        };
        let number = |key: &str| {
            row[key]
                .as_str()
                .and_then(|value| value.parse::<f64>().ok())
                .map_or(0, |value| value as i64)
        };
        // Newer year first, then the lowest card number.
        let rank = (number("year"), -number("card"));
        if best.get(pin).is_none_or(|(current, _)| rank > *current) {
            best.insert(pin.to_owned(), (rank, row));
        }
    }
    best.into_iter()
        .map(|(pin, (_, row))| {
            let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
            (
                pin,
                Characteristics {
                    wall: text("char_ext_wall"),
                    roof: text("char_roof_cnst"),
                    garage: text("char_gar1_cnst"),
                },
            )
        })
        .collect()
}

fn assign(
    geojson: &mut Value,
    parcels: &[([f64; 2], &Characteristics)],
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for feature in geojson["features"].as_array_mut().into_iter().flatten() {
        let properties = &feature["properties"];
        if properties.get("material").is_some() || properties.get("kind").is_some() {
            continue;
        }
        let ring = feature["geometry"]["coordinates"][0]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|point| Some([point[0].as_f64()?, point[1].as_f64()?]))
            .collect::<Vec<_>>();
        if ring.len() < 4 {
            continue;
        }
        let centre = ring[..ring.len() - 1]
            .iter()
            .fold([0.0, 0.0], |sum, point| {
                [sum[0] + point[0], sum[1] + point[1]]
            })
            .map(|sum| sum / (ring.len() - 1) as f64);
        let frame = Projection {
            center: [centre[1], centre[0]],
        };
        let local = ring
            .iter()
            .map(|point| frame.project(point[0], point[1]))
            .collect::<Vec<_>>();
        let area = local
            .windows(2)
            .map(|pair| pair[0][0] * pair[1][1] - pair[1][0] * pair[0][1])
            .sum::<f64>()
            .abs()
            * 0.5;
        let garage = matches!(
            properties["building"].as_str(),
            Some("garage" | "garages" | "shed" | "carport")
        ) || area < GARAGE_MAX_AREA_M2;
        let located = parcels
            .iter()
            .map(|(location, characteristics)| {
                (frame.project(location[0], location[1]), *characteristics)
            })
            .collect::<Vec<_>>();
        let contained = (!garage)
            .then(|| located.iter().find(|(point, _)| inside(*point, &local)))
            .flatten();
        let nearest = || {
            located
                .iter()
                .map(|(point, characteristics)| (point[0].hypot(point[1]), *characteristics))
                .filter(|(distance, _)| *distance <= MATCH_RADIUS_M)
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, characteristics)| characteristics)
        };
        let matched = contained
            .map(|(_, characteristics)| *characteristics)
            .or_else(nearest);
        let (material, roof, source) = match matched {
            Some(characteristics) if garage => {
                (garage_material(characteristics), "roof_shingle", "assessor")
            }
            Some(characteristics) => (
                facade_material(&characteristics.wall),
                roof_material(&characteristics.roof),
                "assessor",
            ),
            None if area >= UNASSESSED_MASONRY_MIN_AREA_M2 => {
                ("masonry_facade", "roof_gravel", "unassessed_masonry")
            }
            None => continue,
        };
        *counts.entry(material.to_owned()).or_default() += 1;
        feature["properties"]["material"] = json!(material);
        feature["properties"]["roof_material"] = json!(roof);
        feature["properties"]["material_source"] = json!(source);
    }
    counts
}

fn facade_material(wall: &str) -> &'static str {
    match wall {
        "Masonry" => "masonry_facade",
        "Stucco" => "stucco_facade",
        // Frame, and frame-plus-masonry, whose sides and rear are frame.
        _ => "frame_facade",
    }
}

fn roof_material(roof: &str) -> &'static str {
    match roof {
        "Tar + Gravel" => "roof_gravel",
        _ => "roof_shingle",
    }
}

fn garage_material(characteristics: &Characteristics) -> &'static str {
    let construction = if characteristics.garage.is_empty() {
        characteristics.wall.as_str()
    } else {
        characteristics.garage.as_str()
    };
    if construction == "Masonry" {
        "garage_masonry"
    } else {
        "garage_frame"
    }
}

fn inside([x, y]: [f64; 2], ring: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for pair in ring.windows(2) {
        let ([x1, y1], [x2, y2]) = (pair[0], pair[1]);
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn houses_take_their_parcel_and_garages_the_nearest_garage_record() {
        // A 2-flat at the street and its garage 30 m back on the alley; the
        // parcel centroid sits in the house's rear half.
        let square = |lon: f64, lat: f64, half_lon: f64, half_lat: f64| {
            json!([[
                [lon - half_lon, lat - half_lat],
                [lon + half_lon, lat - half_lat],
                [lon + half_lon, lat + half_lat],
                [lon - half_lon, lat + half_lat],
                [lon - half_lon, lat - half_lat]
            ]])
        };
        let mut geojson = json!({"features": [
            {"id": "way/1", "properties": {"building": "yes"},
             "geometry": {"type": "Polygon", "coordinates": square(-87.673, 41.9655, 0.00005, 0.00008)}},
            {"id": "way/2", "properties": {"building": "yes"},
             "geometry": {"type": "Polygon", "coordinates": square(-87.673, 41.96525, 0.00004, 0.00003)}},
            {"id": "way/3", "properties": {"building": "yes", "material": "glass"},
             "geometry": {"type": "Polygon", "coordinates": square(-87.6735, 41.9655, 0.00005, 0.00008)}},
            {"id": "way/4", "properties": {"building": "apartments"},
             "geometry": {"type": "Polygon", "coordinates": square(-87.676, 41.9655, 0.0001, 0.0001)}},
        ]});
        let rows = vec![
            json!({"pin": "1", "year": "2025.0", "card": "1.0", "char_ext_wall": "Frame",
                "char_roof_cnst": "Shingle + Asphalt", "char_gar1_cnst": "Frame"}),
            json!({"pin": "1", "year": "2026.0", "card": "1.0", "char_ext_wall": "Masonry",
                "char_roof_cnst": "Tar + Gravel", "char_gar1_cnst": "Masonry"}),
            json!({"pin": "1", "year": "2026.0", "card": "2.0", "char_ext_wall": "Stucco"}),
        ];
        let characteristics = latest_characteristics(&rows);
        let parcels = vec![([-87.673, 41.96545], &characteristics["1"])];
        let counts = assign(&mut geojson, &parcels);
        let property =
            |index: usize, key: &str| geojson["features"][index]["properties"][key].clone();
        assert_eq!(property(0, "material"), "masonry_facade");
        assert_eq!(property(0, "roof_material"), "roof_gravel");
        assert_eq!(property(1, "material"), "garage_masonry");
        assert_eq!(property(2, "material"), "glass");
        // An unassessed ~370 m² block is masonry with a flat roof.
        assert_eq!(property(3, "material"), "masonry_facade");
        assert_eq!(property(3, "material_source"), "unassessed_masonry");
        assert_eq!(counts.values().sum::<usize>(), 3);
        let parcels = br#"[{"pin":"1","class":"211","lon":"-87.673","lat":"41.96545"},
            {"pin":"2","class":"299","lon":"-87.673","lat":"41.96545"},
            {"pin":"3","class":"517","lon":"-87.673","lat":"41.96545"}]"#;
        assert_eq!(
            parse_parcels(parcels).unwrap().keys().collect::<Vec<_>>(),
            ["1"]
        );
    }
}
