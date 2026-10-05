//! Walked-route input for `--corridor`: GPX tracks/routes or GeoJSON lines,
//! projected to the build's ENU frame as a [`ProbeCorridor`].

use std::path::Path;

use fightbox_steam_audio::ProbeCorridor;
use serde_json::Value;

use crate::city_place::Projection;
use crate::error::{CliError, Result};

/// Island radius for a GeoJSON Point or GPX waypoint without `radius_m`.
const DEFAULT_ISLAND_RADIUS_M: f32 = 20.0;

/// Routes and islands in WGS84 `[longitude, latitude]` degrees.
#[derive(Debug, Default, PartialEq)]
struct Corridor {
    routes: Vec<Vec<[f64; 2]>>,
    islands: Vec<([f64; 2], f32)>,
}

pub(super) fn load(
    path: &Path,
    half_width_m: f32,
    projection: Projection,
) -> Result<ProbeCorridor> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| CliError::new(format!("cannot read {}: {error}", path.display())))?;
    let corridor = if text.trim_start().starts_with('{') {
        parse_geojson(&text)?
    } else {
        parse_gpx(&text)?
    };
    if corridor.routes.iter().all(|route| route.is_empty()) && corridor.islands.is_empty() {
        return Err(CliError::new(format!(
            "{} holds no route points (GPX trkpt/rtept/wpt or GeoJSON LineString/Point)",
            path.display()
        )));
    }
    let project = |point: [f64; 2]| {
        projection
            .project(point[0], point[1])
            .map(|value| value as f32)
    };
    Ok(ProbeCorridor {
        routes_enu_m: corridor
            .routes
            .into_iter()
            .filter(|route| !route.is_empty())
            .map(|route| route.into_iter().map(project).collect())
            .collect(),
        half_width_m,
        islands_enu_m: corridor
            .islands
            .into_iter()
            .map(|(centre, radius)| (project(centre), radius))
            .collect(),
    })
}

fn parse_geojson(text: &str) -> Result<Corridor> {
    let root: Value = serde_json::from_str(text)
        .map_err(|error| CliError::new(format!("invalid corridor GeoJSON: {error}")))?;
    let features = match root["type"].as_str() {
        Some("FeatureCollection") => root["features"].as_array().cloned().unwrap_or_default(),
        Some("Feature") => vec![root.clone()],
        _ => vec![json_feature(root.clone())],
    };
    let mut corridor = Corridor::default();
    for feature in &features {
        let geometry = &feature["geometry"];
        let radius = feature["properties"]["radius_m"]
            .as_f64()
            .map_or(DEFAULT_ISLAND_RADIUS_M, |radius| radius as f32);
        match geometry["type"].as_str() {
            Some("LineString") => corridor.routes.push(positions(&geometry["coordinates"])?),
            Some("MultiLineString") => {
                for line in geometry["coordinates"].as_array().into_iter().flatten() {
                    corridor.routes.push(positions(line)?);
                }
            }
            Some("Point") => corridor
                .islands
                .push((position(&geometry["coordinates"])?, radius)),
            Some("MultiPoint") => {
                for point in positions(&geometry["coordinates"])? {
                    corridor.islands.push((point, radius));
                }
            }
            _ => {}
        }
    }
    Ok(corridor)
}

fn json_feature(geometry: Value) -> Value {
    serde_json::json!({"type": "Feature", "properties": {}, "geometry": geometry})
}

fn positions(value: &Value) -> Result<Vec<[f64; 2]>> {
    value
        .as_array()
        .ok_or_else(|| CliError::new("corridor coordinates must be an array"))?
        .iter()
        .map(position)
        .collect()
}

fn position(value: &Value) -> Result<[f64; 2]> {
    match (value[0].as_f64(), value[1].as_f64()) {
        (Some(lon), Some(lat))
            if (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat) =>
        {
            Ok([lon, lat])
        }
        _ => Err(CliError::new(
            "corridor position must be [longitude, latitude]",
        )),
    }
}

/// Reads `trkseg` and `rte` point runs as routes and `wpt` as islands. A
/// tag scan suffices for the attribute-only points GPX loggers write.
fn parse_gpx(text: &str) -> Result<Corridor> {
    let mut corridor = Corridor::default();
    let mut current: Option<Vec<[f64; 2]>> = None;
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else {
            break;
        };
        let tag = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let name = name.rsplit(':').next().unwrap_or(name);
        match (tag.starts_with('/'), name) {
            (false, "trkseg" | "rte") => current = Some(Vec::new()),
            (true, "trkseg" | "rte") => corridor.routes.extend(current.take()),
            (false, "trkpt" | "rtept") => {
                if let Some(route) = current.as_mut() {
                    route.push(gpx_point(tag)?);
                }
            }
            (false, "wpt") => corridor
                .islands
                .push((gpx_point(tag)?, DEFAULT_ISLAND_RADIUS_M)),
            _ => {}
        }
    }
    corridor.routes.extend(current);
    Ok(corridor)
}

fn gpx_point(tag: &str) -> Result<[f64; 2]> {
    let attribute = |key: &str| {
        let at = tag.find(&format!("{key}="))? + key.len() + 1;
        let quote = tag[at..].chars().next()?;
        let value = &tag[at + 1..];
        value[..value.find(quote)?].trim().parse::<f64>().ok()
    };
    position(&serde_json::json!([attribute("lon"), attribute("lat")]))
        .map_err(|_| CliError::new(format!("GPX point needs numeric lat and lon: <{tag}>")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpx_tracks_routes_and_waypoints_become_routes_and_islands() {
        let gpx = r#"<?xml version="1.0"?><gpx><wpt lat="41.9" lon="-87.6"/>
            <trk><trkseg><trkpt lat="41.90" lon="-87.60"><ele>180</ele></trkpt>
            <trkpt lon='-87.61' lat='41.91'/></trkseg></trk>
            <rte><rtept lat="41.92" lon="-87.62"/></rte></gpx>"#;
        let corridor = parse_gpx(gpx).unwrap();
        assert_eq!(
            corridor.routes,
            vec![
                vec![[-87.60, 41.90], [-87.61, 41.91]],
                vec![[-87.62, 41.92]]
            ]
        );
        assert_eq!(
            corridor.islands,
            vec![([-87.6, 41.9], DEFAULT_ISLAND_RADIUS_M)]
        );
    }

    #[test]
    fn geojson_lines_and_points_with_radius_project_to_enu() {
        let geojson = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[-87.6,41.9],[-87.6,41.901]]}},
            {"type":"Feature","properties":{"radius_m":35},"geometry":{"type":"Point","coordinates":[-87.6,41.9]}}]}"#;
        let corridor = parse_geojson(geojson).unwrap();
        assert_eq!(corridor.routes.len(), 1);
        assert_eq!(corridor.islands, vec![([-87.6, 41.9], 35.0)]);
        let directory =
            std::env::temp_dir().join(format!("fightbox-corridor-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("walk.geojson");
        std::fs::write(&path, geojson).unwrap();
        let projected = load(
            &path,
            20.0,
            Projection {
                center: [41.9, -87.6],
            },
        )
        .unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        let north = projected.routes_enu_m[0][1];
        assert!(
            north[0].abs() < 1.0e-3 && (north[1] - 111.2).abs() < 0.2,
            "{north:?}"
        );
        assert_eq!(projected.half_width_m, 20.0);
        assert!(parse_gpx("<gpx></gpx>").unwrap().routes.is_empty());
    }
}
