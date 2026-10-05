//! Roof-part authority for outlines flattened by incomplete OSM/LiDAR heights.
//! The current solid-outline mesh uses the tallest substantial roof part.
//! Adding overlapping part solids would leave the outline's false roof inside
//! the building, so exact per-part extrusions await footprint splitting.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::city_place::Projection;
use crate::error::{CliError, Result};

#[derive(Default)]
pub(super) struct Parts {
    features: Vec<Value>,
    fetched_count: usize,
    excluded_antennas: Vec<String>,
}

pub(super) fn request([south, west, north, east]: [f64; 4]) -> String {
    let bounds = format!("{south},{west},{north},{east}");
    format!(
        "[out:json][timeout:90];\n(\n  way[\"building:part\"]({bounds});\n  relation[\"building:part\"]({bounds});\n);\nout body;\n>;\nout skel qt;\n"
    )
}

pub(super) fn parse(raw: &[u8]) -> Result<Parts> {
    let mut root: Value = serde_json::from_slice(raw)
        .map_err(|error| CliError::new(format!("invalid building parts JSON: {error}")))?;
    let elements = root["elements"]
        .as_array_mut()
        .ok_or_else(|| CliError::new("building parts response needs elements"))?;
    let mut tags = BTreeMap::new();
    for element in elements {
        if !matches!(element["type"].as_str(), Some("way" | "relation")) {
            continue;
        }
        let original = element["tags"].clone();
        if let Some(object) = element["tags"].as_object_mut() {
            object.remove("building");
        }
        if original.get("building:part").is_none() || original["building:part"] == "no" {
            continue;
        }
        let id = format!("{}/{}", element["type"].as_str().unwrap(), element["id"]);
        tags.insert(id, original.clone());
        // Only parts with a known height can be an acoustic authority.
        let height = super::numeric_tag(&original, "height", true)
            .and_then(|height| height.as_f64())
            .or_else(|| {
                super::numeric_tag(&original, "building:levels", false)
                    .and_then(|levels| levels.as_f64())
                    .map(|levels| levels * 3.5)
            });
        if let Some(height) = height
            .filter(|height| height.is_finite() && *height > 0.0 && *height < f64::from(f32::MAX))
        {
            element["tags"]["building"] = json!("yes");
            element["tags"]["height"] = json!(height.to_string());
        }
    }
    let fetched_count = tags.len();
    if !root["elements"]
        .as_array()
        .unwrap()
        .iter()
        .any(|element| element["tags"].get("building").is_some())
    {
        return Ok(Parts {
            fetched_count,
            ..Parts::default()
        });
    }
    let converted = super::convert_osm(
        &serde_json::to_vec(&root).map_err(|error| CliError::new(error.to_string()))?,
    )?;
    let mut result = Parts {
        fetched_count,
        ..Parts::default()
    };
    for feature in converted["features"].as_array().unwrap() {
        let id = feature["id"].as_str().unwrap();
        let original = &tags[id.split(':').next().unwrap()];
        if antenna(feature, original) {
            result.excluded_antennas.push(id.to_owned());
        } else {
            result.features.push(feature.clone());
        }
    }
    Ok(result)
}

fn local_ring(feature: &Value, projection: Projection) -> Vec<[f64; 2]> {
    feature["geometry"]["coordinates"][0]
        .as_array()
        .unwrap()
        .iter()
        .map(|point| {
            projection
                .project(point[0].as_f64().unwrap(), point[1].as_f64().unwrap())
                .map(f64::from)
        })
        .collect()
}

fn antenna(feature: &Value, tags: &Value) -> bool {
    if ["man_made", "building:part", "tower:type"]
        .iter()
        .any(|key| {
            tags[*key]
                .as_str()
                .is_some_and(|value| matches!(value, "antenna" | "mast" | "communication"))
        })
    {
        return true;
    }
    let first = &feature["geometry"]["coordinates"][0][0];
    let projection = Projection {
        center: [first[1].as_f64().unwrap(), first[0].as_f64().unwrap()],
    };
    let ring = local_ring(feature, projection);
    let area = ring
        .windows(2)
        .map(|edge| edge[0][0] * edge[1][1] - edge[1][0] * edge[0][1])
        .sum::<f64>()
        .abs()
        * 0.5;
    let height = feature["properties"]["height"].as_f64().unwrap();
    let roof = super::numeric_tag(tags, "roof:height", true)
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    // Some OSM antennas have only generic building-part and roof tags (the
    // recorded Willis masts do). A tiny footprint topped by a >20 m spire is
    // not a substantial acoustic facade. Do not blacklist landmark OSM IDs.
    area < 25.0 && height > 50.0 && roof > 20.0
}

fn contains(point: [f64; 2], ring: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for edge in ring.windows(2) {
        let [a, b] = [edge[0], edge[1]];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let norm = dx * dx + dy * dy;
        let t = if norm > 0.0 {
            ((point[0] - a[0]) * dx + (point[1] - a[1]) * dy) / norm
        } else {
            0.0
        }
        .clamp(0.0, 1.0);
        // Shared nodes and minor OSM alignment discrepancies belong to the
        // outline too. One metre cannot pull an unrelated street across it.
        if (point[0] - a[0] - t * dx).hypot(point[1] - a[1] - t * dy) <= 1.0 {
            return true;
        }
        if (a[1] > point[1]) != (b[1] > point[1]) && point[0] < a[0] + dx * (point[1] - a[1]) / dy {
            inside = !inside;
        }
    }
    inside
}

pub(super) fn apply(geojson: &mut Value, parts: &Parts) -> Result<Value> {
    let mut selected = Vec::new();
    let mut corroborated = Vec::new();
    for outline in geojson["features"]
        .as_array_mut()
        .expect("converted features")
    {
        let first = &outline["geometry"]["coordinates"][0][0];
        let projection = Projection {
            center: [first[1].as_f64().unwrap(), first[0].as_f64().unwrap()],
        };
        let ring = local_ring(outline, projection);
        let matches = parts
            .features
            .iter()
            .filter(|part| {
                local_ring(part, projection)
                    .iter()
                    .all(|point| contains(*point, &ring))
            })
            .collect::<Vec<_>>();
        let Some(tallest) = matches.iter().max_by(|a, b| {
            a["properties"]["height"]
                .as_f64()
                .unwrap()
                .total_cmp(&b["properties"]["height"].as_f64().unwrap())
        }) else {
            continue;
        };
        let height = tallest["properties"]["height"].as_f64().unwrap();
        let building_id = outline["id"].clone();
        let props = &mut outline["properties"];
        // Preserve measurements agreeing within 10% (Chase: 259 m mapped
        // part vs 246.4 m DSM median). A much lower part can be a lone annex
        // in an incomplete part map, so it must not flatten a measured tower.
        // Otherwise the mapped roof, rather than an outline-wide DSM median,
        // supplies the height, including stepped towers such as Franklin.
        if let Some(measured) = props["lidar_height_m"].as_f64() {
            if (height - measured).abs() <= height * 0.1 || height < measured * 0.5 {
                corroborated.push(json!({"building_id": building_id,
                    "measured_height_m": measured, "tallest_roof_part_height_m": height,
                    "tallest_roof_part_id": tallest["id"], "decision": "retain LiDAR: agreement within 10% or low annex part"}));
                continue;
            }
        }
        props["height"] = json!(height);
        props["height_source"] = json!("building_parts");
        props["height_part_id"] = tallest["id"].clone();
        // A parts decision supersedes an outline's OSM/LiDAR plausibility
        // decision; the measured height remains available for comparison.
        props
            .as_object_mut()
            .unwrap()
            .remove("osm_plausibility_fallback");
        selected.push(
            json!({"building_id": outline["id"], "name": outline["properties"]["name"],
            "height_m": height, "tallest_roof_part_id": tallest["id"],
            "part_ids": matches.iter().map(|part| part["id"].clone()).collect::<Vec<_>>(),
            "lidar_height_m": outline["properties"]["lidar_height_m"]}),
        );
    }
    Ok(
        json!({"fetched_count": parts.fetched_count, "roof_height_count": parts.features.len(),
        "excluded_antennas": parts.excluded_antennas, "buildings": selected, "corroborated_buildings": corroborated,
        "geometry_policy": "tallest substantial roof part; LiDAR retained for agreement within 10% or a low annex part; antennas excluded"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded_parts() -> Parts {
        parse(include_bytes!(
            "../../../../fixtures/city/height-parts/willis-parts.json"
        ))
        .unwrap()
    }

    #[test]
    fn parses_recorded_parts_and_excludes_527m_antennas() {
        let parts = recorded_parts();
        assert_eq!(parts.fetched_count, 7);
        assert_eq!(parts.features.len(), 5);
        let willis = parts
            .features
            .iter()
            .find(|part| part["id"] == "way/137162296")
            .unwrap();
        assert_eq!(willis["properties"]["height"], 442.0);
        assert_eq!(parts.excluded_antennas, ["way/233918141", "way/233918220"]);
        assert!(request([1.0, 2.0, 3.0, 4.0]).contains("relation[\"building:part\"](1,2,3,4)"));
    }

    #[test]
    fn willis_uses_442m_roof_and_chase_keeps_measured_height() {
        let mut outlines = super::super::convert_osm(include_bytes!(
            "../../../../fixtures/city/height-parts/landmark-outlines.json"
        ))
        .unwrap();
        let measured = BTreeMap::from([
            ("way/380868216".to_owned(), 19.5),
            ("way/230613007".to_owned(), 246.0),
        ]);
        super::super::apply_height_overrides(&mut outlines, &measured).unwrap();
        let summary = apply(&mut outlines, &recorded_parts()).unwrap();
        let features = outlines["features"].as_array().unwrap();
        let willis = features
            .iter()
            .find(|feature| feature["id"] == "way/380868216")
            .unwrap();
        let chase = features
            .iter()
            .find(|feature| feature["id"] == "way/230613007")
            .unwrap();
        assert_eq!(willis["properties"]["height"], 442.0);
        assert_eq!(willis["properties"]["height_part_id"], "way/137162296");
        assert_eq!(chase["properties"]["height"], 246.0);
        let coverage = super::super::height_coverage(&outlines, Some(&measured));
        assert_eq!(coverage["parts_count"], 1);
        assert_eq!(coverage["lidar_count"], 1);
        assert_eq!(summary["buildings"][0]["height_m"], 442.0);
    }

    #[test]
    fn neighborhood_house_keeps_its_measured_height_without_parts() {
        let mut house = super::super::convert_osm(include_bytes!(
            "../../../../fixtures/city/height-parts/neighborhood-house.json"
        ))
        .unwrap();
        let id = house["features"][0]["id"].as_str().unwrap().to_owned();
        let measured = BTreeMap::from([(id, 9.4)]);
        super::super::apply_height_overrides(&mut house, &measured).unwrap();
        let before = house.clone();
        let empty = parse(include_bytes!(
            "../../../../fixtures/city/height-parts/neighborhood-parts.json"
        ))
        .unwrap();
        assert_eq!(apply(&mut house, &empty).unwrap()["fetched_count"], 0);
        assert_eq!(house, before);
        assert_eq!(house["features"][0]["properties"]["height"], 9.4);
    }

    #[test]
    fn relation_parts_and_levels_parse_without_inventing_height() {
        let raw = json!({"elements": [
            {"type":"node", "id":1, "lon":0, "lat":0},
            {"type":"node", "id":2, "lon":0.001, "lat":0},
            {"type":"node", "id":3, "lon":0, "lat":0.001},
            {"type":"way", "id":4, "nodes":[1,2,3,1]},
            {"type":"relation", "id":5, "tags":{"building:part":"yes", "building:levels":"10"},
                "members":[{"type":"way", "ref":4, "role":"outer"}]}
        ]});
        let parts = parse(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(parts.features[0]["id"], "relation/5");
        assert_eq!(parts.features[0]["properties"]["height"], 35.0);
        assert!(
            parse(br#"{"elements": [{"type":"way", "id":1, "tags":{"building:part":"yes"}}]}"#)
                .unwrap()
                .features
                .is_empty()
        );
        assert!(parse(br#"{}"#).is_err());
    }
}
