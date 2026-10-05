//! `--graded` probe density: the full 4 m lattice where the listener walks
//! (scene centre, `--corridor` routes, street corners) and its nested sparse
//! subset elsewhere, as a [`GradedDensity`] for the bake's probe mask.

use std::collections::BTreeMap;

use fightbox_steam_audio::{GradedDensity, ProbeCorridor};
use serde_json::Value;

use crate::city_place::Projection;
use crate::error::{CliError, Result};

/// Full density around each street corner, so a path bending round it keeps
/// the fine probes it was baked through. Covers a 20 m Chicago right of way.
pub(super) const CORNER_RADIUS_M: f32 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct GradedOptions {
    /// Full density within this distance of the scene centre.
    pub fine_radius_m: f32,
    /// Spacing of the sparse layer; a whole multiple of the probe spacing.
    pub coarse_spacing_m: f32,
}

impl GradedOptions {
    pub(super) const DEFAULT: Self = Self {
        fine_radius_m: 60.0,
        coarse_spacing_m: 8.0,
    };

    /// Lattice steps per sparse step, refusing a spacing off the lattice.
    pub(super) fn stride(self, probe_spacing_m: f32) -> Result<u32> {
        let stride = (self.coarse_spacing_m / probe_spacing_m).round();
        if !(2.0..=8.0).contains(&stride)
            || (stride * probe_spacing_m - self.coarse_spacing_m).abs() > 1.0e-3
        {
            return Err(CliError::new(format!(
                "--coarse-spacing-m must be 2 to 8 times the {probe_spacing_m} m probe spacing"
            )));
        }
        Ok(stride as u32)
    }
}

/// The graded density for a build: the centre disk and corner disks, plus
/// the walked routes and islands of `corridor` when one was given.
pub(super) fn density(
    options: GradedOptions,
    stride: u32,
    corridor: Option<ProbeCorridor>,
    corners: &[[f32; 2]],
) -> GradedDensity {
    let mut fine = corridor.unwrap_or(ProbeCorridor {
        routes_enu_m: Vec::new(),
        half_width_m: CORNER_RADIUS_M,
        islands_enu_m: Vec::new(),
    });
    fine.islands_enu_m.push(([0.0, 0.0], options.fine_radius_m));
    fine.islands_enu_m
        .extend(corners.iter().map(|corner| (*corner, CORNER_RADIUS_M)));
    GradedDensity {
        coarse_stride: stride,
        fine,
    }
}

/// Street corners inside `bbox`: OSM nodes where three or more road segments
/// meet. Alleys count, since a sound bends round an alley mouth too; sidewalks
/// and crossings do not, or every block face would read as a corner.
pub(super) fn street_corners(
    raw: &[u8],
    projection: Projection,
    bbox: [f64; 4],
) -> Result<Vec<[f32; 2]>> {
    let root: Value =
        serde_json::from_slice(raw).map_err(|error| CliError::new(error.to_string()))?;
    let elements = root["elements"]
        .as_array()
        .ok_or_else(|| CliError::new("OSM elements missing"))?;
    let mut degree: BTreeMap<i64, u32> = BTreeMap::new();
    for way in elements
        .iter()
        .filter(|element| element["type"] == "way" && is_road(&element["tags"]))
    {
        let Some(nodes) = way["nodes"].as_array() else {
            continue;
        };
        let last = nodes.len().saturating_sub(1);
        for (position, node) in nodes.iter().enumerate() {
            if let Some(id) = node.as_i64() {
                *degree.entry(id).or_default() += if position == 0 || position == last { 1 } else { 2 };
            }
        }
    }
    let [south, west, north, east] = bbox;
    Ok(elements
        .iter()
        .filter(|element| {
            element["type"] == "node"
                && element["id"]
                    .as_i64()
                    .is_some_and(|id| degree.get(&id).is_some_and(|count| *count >= 3))
        })
        .filter_map(|node| Some((node["lon"].as_f64()?, node["lat"].as_f64()?)))
        .filter(|(longitude, latitude)| {
            (south..=north).contains(latitude) && (west..=east).contains(longitude)
        })
        .map(|(longitude, latitude)| {
            projection
                .project(longitude, latitude)
                .map(|value| value as f32)
        })
        .collect())
}

fn is_road(tags: &Value) -> bool {
    if tags["bridge"] == "yes" || tags["tunnel"] == "yes" {
        return false;
    }
    match tags["highway"].as_str() {
        Some(
            "primary" | "primary_link" | "secondary" | "secondary_link" | "tertiary"
            | "tertiary_link" | "residential" | "unclassified" | "living_street",
        ) => true,
        Some("service") => tags["service"] == "alley",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn corners_are_road_junctions_not_way_splits_or_sidewalk_crossings() {
        // A residential cross street meeting a residential street that OSM split
        // in two ways at the junction, an alley teeing into the far half, a
        // collinear split with no side road, and a footway crossing.
        let node = |id: i64, lon: f64| json!({"type": "node", "id": id, "lat": 41.96, "lon": lon});
        let way = |id: i64, tags: Value, nodes: &[i64]| {
            json!({"type": "way", "id": id, "tags": tags, "nodes": nodes})
        };
        let residential = json!({"highway": "residential"});
        let raw = json!({"elements": [
            node(1, -87.673), node(2, -87.672), node(3, -87.671), node(4, -87.670),
            node(5, -87.669), json!({"type": "node", "id": 6, "lat": 41.961, "lon": -87.672}),
            json!({"type": "node", "id": 7, "lat": 41.961, "lon": -87.670}),
            json!({"type": "node", "id": 8, "lat": 41.961, "lon": -87.669}),
            way(10, residential.clone(), &[1, 2]),
            way(11, residential.clone(), &[2, 3, 4]),
            way(12, residential.clone(), &[4, 5]),
            way(13, residential, &[6, 2]),
            way(14, json!({"highway": "service", "service": "alley"}), &[7, 4]),
            way(15, json!({"highway": "footway", "footway": "crossing"}), &[8, 5]),
            way(16, json!({"highway": "footway", "footway": "sidewalk"}), &[5, 3]),
        ]});
        let projection = Projection { center: [41.96, -87.671] };
        let corners = street_corners(
            serde_json::to_vec(&raw).unwrap().as_slice(),
            projection,
            [41.95, -87.68, 41.97, -87.66],
        )
        .unwrap();
        let expected = [-87.672, -87.670].map(|longitude| {
            projection.project(longitude, 41.96).map(|value| value as f32)
        });
        assert_eq!(corners, expected);
        let graded = density(GradedOptions::DEFAULT, 2, None, &corners);
        assert_eq!(graded.fine.islands_enu_m.len(), 3);
        assert_eq!(graded.fine.islands_enu_m[0], ([0.0, 0.0], 60.0));
        assert_eq!(GradedOptions::DEFAULT.stride(4.0).unwrap(), 2);
        let off_lattice = GradedOptions { coarse_spacing_m: 6.0, ..GradedOptions::DEFAULT };
        assert!(off_lattice.stride(4.0).is_err());
    }
}
