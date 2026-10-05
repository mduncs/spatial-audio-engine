//! Street detail beyond building footprints, emitted as extra GeoJSON
//! features carrying a `kind` so height statistics can tell them apart:
//!
//! - `fence`: thin two-sided walls where LiDAR shows a continuous 1–2.5 m
//!   return along an alley edge. Sparse returns (chain-link, a parked car)
//!   and gangway gaps stay open.
//! - `rail_embankment`: the fill between retaining walls under at-grade-free
//!   track, ending at each bridge so cross streets keep their underpasses.
//! - `rail_deck`: a raised, two-sided (`min_height`) solid: one per Metra
//!   underpass (an opaque envelope, ballast on top), and one girder-and-tie
//!   strip per elevated track, leaving the real opening between tracks; a
//!   continuous slab would make Steam's reflection visibility opaque there.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::Street;
use super::lidar::Rasters;
use crate::city_place::Projection;

const ALONG_STEP_M: f64 = 0.5;
const SCAN_FROM_M: f64 = 2.0;
const SCAN_TO_M: f64 = 6.0;
const SCAN_STEP_M: f64 = 0.25;
const FENCE_RETURN_M: [f64; 2] = [1.0, 2.5];
/// A return this tall before any fence is a garage, house, or trunk.
const TALL_RETURN_M: f64 = 2.6;
/// A fence is thin: one metre further out the return must have ended.
const THIN_PROBE_M: f64 = 1.0;
const MAX_OFFSET_JUMP_M: f64 = 0.75;
const MAX_GAP_M: f64 = 1.0;
const MIN_FENCE_M: f64 = 3.0;
const FENCE_THICKNESS_M: f64 = 0.15;
const FENCE_HEIGHT_M: [f64; 2] = [1.2, 2.4];

const RAIL_STEP_M: f64 = 5.0;
const PARALLEL_TRACK_M: f64 = 15.0;
const WALL_REACH_M: f64 = 25.0;
const TRACK_HALF_WIDTH_M: f64 = 3.0;
/// Girders and ties under one elevated track: about 2.6 m wide.
const ELEVATED_HALF_WIDTH_M: f64 = 1.3;
const DECK_THICKNESS_M: f64 = 1.0;
/// Below this the track is at grade and needs no solid.
const MIN_EMBANKMENT_M: f64 = 1.5;
/// Astra-accepted Metra wall height when no LiDAR measures it.
const DEFAULT_EMBANKMENT_M: f64 = 3.7;
const DEFAULT_ELEVATED_DECK_M: f64 = 5.5;
const DEFAULT_BRIDGE_DECK_M: f64 = 4.7;

pub(super) const FENCE_MATERIAL: &str = "wood_fence";
const WALL_MATERIAL: &str = "concrete";
const BALLAST_MATERIAL: &str = "ballast";
const ELEVATED_DECK_MATERIAL: &str = "rail_deck";
const BRIDGE_DECK_MATERIAL: &str = "concrete";

/// Building footprints in the build frame, bucketed for point queries.
pub(super) struct Footprints {
    rings: Vec<Vec<[f64; 2]>>,
    cells: HashMap<(i64, i64), Vec<usize>>,
}

const CELL_M: f64 = 10.0;

impl Footprints {
    pub(super) fn from_geojson(geojson: &Value, projection: Projection) -> Self {
        let mut rings = Vec::new();
        let mut cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for feature in geojson["features"].as_array().into_iter().flatten() {
            let ring = feature["geometry"]["coordinates"][0]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|point| {
                    Some(projection.project(point[0].as_f64()?, point[1].as_f64()?))
                })
                .collect::<Vec<_>>();
            if ring.len() < 3 {
                continue;
            }
            let index = rings.len();
            let [min, max] = bounds(&ring);
            for x in cell(min[0])..=cell(max[0]) {
                for y in cell(min[1])..=cell(max[1]) {
                    cells.entry((x, y)).or_default().push(index);
                }
            }
            rings.push(ring);
        }
        Self { rings, cells }
    }

    fn contains(&self, point: [f64; 2]) -> bool {
        self.cells
            .get(&(cell(point[0]), cell(point[1])))
            .is_some_and(|indices| {
                indices
                    .iter()
                    .any(|&index| inside(point, &self.rings[index]))
            })
    }
}

/// Fences along every `service=alley` segment, both sides.
pub(super) fn fences(
    streets: &[Street],
    footprints: &Footprints,
    rasters: &Rasters,
    projection: Projection,
) -> Vec<Value> {
    let above = |point: [f64; 2]| {
        let [latitude, longitude] = projection.unproject(point[0], point[1]);
        rasters.above_ground_m(longitude, latitude)
    };
    let mut features = Vec::new();
    for street in streets.iter().filter(|street| street.service == "alley") {
        for (segment_index, segment) in street.points.windows(2).enumerate() {
            let a = segment[0].map(f64::from);
            let b = segment[1].map(f64::from);
            let length = (b[0] - a[0]).hypot(b[1] - a[1]);
            if length < MIN_FENCE_M {
                continue;
            }
            let along = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
            for (side_name, side) in [("l", 1.0), ("r", -1.0)] {
                let normal = [-along[1] * side, along[0] * side];
                let at = |s: f64, d: f64| {
                    [
                        a[0] + along[0] * s + normal[0] * d,
                        a[1] + along[1] * s + normal[1] * d,
                    ]
                };
                let samples = (0..=(length / ALONG_STEP_M) as usize)
                    .map(|index| {
                        let s = index as f64 * ALONG_STEP_M;
                        edge_return(|d| at(s, d), &above, footprints)
                    })
                    .collect::<Vec<_>>();
                for (run_index, run) in fence_runs(&samples).into_iter().enumerate() {
                    let first = run.first().expect("nonempty run").0 as f64 * ALONG_STEP_M;
                    let last = run.last().expect("nonempty run").0 as f64 * ALONG_STEP_M;
                    let (start, end) = (
                        (first - ALONG_STEP_M * 0.5).max(0.0),
                        (last + ALONG_STEP_M * 0.5).min(length),
                    );
                    let offset = median(run.iter().map(|sample| sample.1).collect());
                    let height = median(run.iter().map(|sample| sample.2).collect())
                        .clamp(FENCE_HEIGHT_M[0], FENCE_HEIGHT_M[1]);
                    let half = FENCE_THICKNESS_M * 0.5;
                    let ring = [
                        at(start, offset - half),
                        at(end, offset - half),
                        at(end, offset + half),
                        at(start, offset + half),
                    ];
                    features.push(polygon(
                        format!("fence/{}:{segment_index}{side_name}{run_index}", street.id),
                        &ring,
                        projection,
                        json!({"kind": "fence", "height": round_dm(height),
                            "material": FENCE_MATERIAL, "source": "lidar"}),
                    ));
                }
            }
        }
    }
    features
}

/// The first fence-like return scanning outward from the alley edge, as
/// `(offset, height)`, or `None` when a footprint, a tall return, or a thick
/// low return (car, hedge, shed) comes first.
fn edge_return(
    at: impl Fn(f64) -> [f64; 2],
    above: &impl Fn([f64; 2]) -> Option<f64>,
    footprints: &Footprints,
) -> Option<(f64, f64)> {
    let steps = ((SCAN_TO_M - SCAN_FROM_M) / SCAN_STEP_M) as usize;
    for step in 0..=steps {
        let offset = SCAN_FROM_M + step as f64 * SCAN_STEP_M;
        let point = at(offset);
        if footprints.contains(point) {
            return None;
        }
        let Some(height) = above(point) else {
            continue;
        };
        if height > TALL_RETURN_M {
            return None;
        }
        if (FENCE_RETURN_M[0]..=FENCE_RETURN_M[1]).contains(&height) {
            let beyond = above(at(offset + THIN_PROBE_M));
            return beyond
                .is_none_or(|beyond| beyond < FENCE_RETURN_M[0])
                .then_some((offset, height));
        }
    }
    None
}

/// Continuous runs of `(sample index, offset, height)`: consecutive hits at
/// a steady offset, bridging at most `MAX_GAP_M` of missing samples, at
/// least `MIN_FENCE_M` long.
fn fence_runs(samples: &[Option<(f64, f64)>]) -> Vec<Vec<(usize, f64, f64)>> {
    let mut runs = Vec::new();
    let mut current: Vec<(usize, f64, f64)> = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        let Some((offset, height)) = *sample else {
            continue;
        };
        if let Some(&(last, last_offset, _)) = current.last() {
            if (index - last) as f64 * ALONG_STEP_M > MAX_GAP_M
                || (offset - last_offset).abs() > MAX_OFFSET_JUMP_M
            {
                runs.push(std::mem::take(&mut current));
            }
        }
        current.push((index, offset, height));
    }
    runs.push(current);
    runs.retain(|run| {
        run.len() >= 2
            && (run[run.len() - 1].0 - run[0].0) as f64 * ALONG_STEP_M >= MIN_FENCE_M - ALONG_STEP_M
    });
    runs
}

#[derive(Clone, Copy, PartialEq)]
enum RailKind {
    /// Track on the ground; becomes an embankment only where raised.
    Track,
    /// Track on a bridge over a street.
    Bridge,
    /// Elevated rapid-transit structure.
    Elevated,
    Wall,
}

struct RailLine {
    id: String,
    kind: RailKind,
    points: Vec<[f64; 2]>,
}

/// Embankments and decks from the Overpass rail and retaining-wall ways,
/// clipped to `[min, max]` in the build frame.
pub(super) fn rail(
    raw: &Value,
    projection: Projection,
    [min, max]: [[f64; 2]; 2],
    rasters: Option<&Rasters>,
) -> (Vec<Value>, Value) {
    let elements = raw["elements"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let nodes = elements
        .iter()
        .filter(|element| element["type"] == "node")
        .filter_map(|node| {
            Some((
                node["id"].as_i64()?,
                projection.project(node["lon"].as_f64()?, node["lat"].as_f64()?),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut lines = Vec::new();
    for way in elements.iter().filter(|element| element["type"] == "way") {
        let tags = &way["tags"];
        let layer = tags["layer"]
            .as_str()
            .and_then(|layer| layer.parse::<i64>().ok())
            .unwrap_or(0);
        let bridge = tags["bridge"].as_str().is_some_and(|bridge| bridge != "no");
        if tags["tunnel"].as_str().is_some_and(|tunnel| tunnel != "no") {
            continue;
        }
        let kind = match tags["railway"].as_str() {
            // CTA maps its elevated lines as raised `subway`.
            Some("subway" | "light_rail") if bridge || layer > 0 => RailKind::Elevated,
            Some("rail") if bridge => RailKind::Bridge,
            Some("rail") => RailKind::Track,
            _ if tags["barrier"] == "retaining_wall" => RailKind::Wall,
            _ => continue,
        };
        let points = way["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| nodes.get(&id.as_i64()?).copied())
            .collect::<Vec<_>>();
        for part in clip_polyline(&points, min, max) {
            lines.push(RailLine {
                id: format!("way/{}", way["id"]),
                kind,
                points: part,
            });
        }
    }
    lines.sort_by(|a, b| {
        length(&b.points)
            .total_cmp(&length(&a.points))
            .then(a.id.cmp(&b.id))
    });

    let at = |point: [f64; 2]| {
        let [latitude, longitude] = projection.unproject(point[0], point[1]);
        [longitude, latitude]
    };
    let mut features = Vec::new();
    let mut summary = Vec::new();
    let mut assigned = vec![false; lines.len()];
    for spine_index in 0..lines.len() {
        let spine = &lines[spine_index];
        if assigned[spine_index]
            || spine.kind == RailKind::Wall
            || length(&spine.points) < RAIL_STEP_M
        {
            continue;
        }
        assigned[spine_index] = true;
        let mut members = vec![(spine.points.as_slice(), half_width(spine.kind))];
        let mut walls = false;
        for (index, line) in lines.iter().enumerate() {
            let reach = match line.kind {
                RailKind::Wall if spine.kind == RailKind::Track => WALL_REACH_M,
                // Elevated tracks stay separate strips with the gap between.
                RailKind::Elevated => continue,
                kind if kind == spine.kind && !assigned[index] => PARALLEL_TRACK_M,
                _ => continue,
            };
            if mostly_within(&line.points, &spine.points, reach) {
                if line.kind == RailKind::Wall {
                    walls = true;
                } else {
                    assigned[index] = true;
                }
                members.push((line.points.as_slice(), half_width(line.kind)));
            }
        }
        let samples = strip(&spine.points, &members);
        if samples.len() < 2 {
            continue;
        }
        let measured = rasters.and_then(|rasters| {
            let heights = samples
                .iter()
                .filter_map(|sample| match spine.kind {
                    RailKind::Track => {
                        let top = rasters.ground_m(at(sample.point)[0], at(sample.point)[1])?;
                        let side = |offset: f64| {
                            let point = sample.offset(offset);
                            rasters.ground_m(at(point)[0], at(point)[1])
                        };
                        let street = side(sample.high + 8.0)?.min(side(sample.low - 8.0)?);
                        Some(top - street)
                    }
                    _ => rasters.above_ground_m(at(sample.point)[0], at(sample.point)[1]),
                })
                .collect::<Vec<_>>();
            (heights.len() * 2 >= samples.len()).then(|| median(heights))
        });
        let (kind, top, base, material, roof) = match spine.kind {
            RailKind::Track => {
                let height = match measured {
                    Some(height) if height >= MIN_EMBANKMENT_M => height.min(8.0),
                    Some(_) => continue,
                    None if walls => DEFAULT_EMBANKMENT_M,
                    None => continue,
                };
                (
                    "rail_embankment",
                    height,
                    None,
                    WALL_MATERIAL,
                    BALLAST_MATERIAL,
                )
            }
            RailKind::Bridge | RailKind::Elevated => {
                let fallback = if spine.kind == RailKind::Bridge {
                    DEFAULT_BRIDGE_DECK_M
                } else {
                    DEFAULT_ELEVATED_DECK_M
                };
                let top = measured
                    .filter(|top| (3.0..=12.0).contains(top))
                    .unwrap_or(fallback);
                let (material, roof) = if spine.kind == RailKind::Bridge {
                    (BRIDGE_DECK_MATERIAL, BALLAST_MATERIAL)
                } else {
                    (ELEVATED_DECK_MATERIAL, ELEVATED_DECK_MATERIAL)
                };
                (
                    "rail_deck",
                    top,
                    Some(top - DECK_THICKNESS_M),
                    material,
                    roof,
                )
            }
            RailKind::Wall => unreachable!("walls are never spines"),
        };
        let ring = samples
            .iter()
            .map(|sample| sample.offset(sample.high))
            .chain(samples.iter().rev().map(|sample| sample.offset(sample.low)))
            .collect::<Vec<_>>();
        let mut properties = json!({"kind": kind, "height": round_dm(top),
            "material": material, "roof_material": roof,
            "source": if measured.is_some() { "lidar" } else { "default" }});
        if let Some(base) = base {
            properties["min_height"] = json!(round_dm(base));
        }
        summary.push(
            json!({"id": spine.id, "kind": kind, "height_m": round_dm(top),
            "length_m": round_dm(length(&spine.points)), "members": members.len()}),
        );
        features.push(polygon(
            format!("rail/{}:{}", spine.id, features.len()),
            &ring,
            projection,
            properties,
        ));
    }
    (features, Value::Array(summary))
}

fn half_width(kind: RailKind) -> f64 {
    match kind {
        RailKind::Track | RailKind::Bridge => TRACK_HALF_WIDTH_M,
        RailKind::Elevated => ELEVATED_HALF_WIDTH_M,
        RailKind::Wall => 0.0,
    }
}

struct StripSample {
    point: [f64; 2],
    normal: [f64; 2],
    low: f64,
    high: f64,
}

impl StripSample {
    fn offset(&self, offset: f64) -> [f64; 2] {
        [
            self.point[0] + self.normal[0] * offset,
            self.point[1] + self.normal[1] * offset,
        ]
    }
}

/// Spine samples every `RAIL_STEP_M`, each widened to cover every member
/// running abeam of it: a track by its half width, a wall exactly.
fn strip(spine: &[[f64; 2]], members: &[(&[[f64; 2]], f64)]) -> Vec<StripSample> {
    resample(spine, RAIL_STEP_M)
        .into_iter()
        .map(|(point, tangent)| {
            let normal = [-tangent[1], tangent[0]];
            let (mut low, mut high) = (f64::INFINITY, f64::NEG_INFINITY);
            for (line, half) in members {
                let nearest = nearest_point(line, point);
                let delta = [nearest[0] - point[0], nearest[1] - point[1]];
                if (delta[0] * tangent[0] + delta[1] * tangent[1]).abs() > 2.0 {
                    continue;
                }
                let offset = delta[0] * normal[0] + delta[1] * normal[1];
                low = low.min(offset - half);
                high = high.max(offset + half);
            }
            StripSample {
                point,
                normal,
                low,
                high,
            }
        })
        .filter(|sample| sample.low < sample.high)
        .collect()
}

/// Points every `step` along `line` (ends included) with unit tangents.
fn resample(line: &[[f64; 2]], step: f64) -> Vec<([f64; 2], [f64; 2])> {
    let total = length(line);
    let count = (total / step).ceil().max(1.0) as usize;
    (0..=count)
        .filter_map(|index| point_along(line, total * index as f64 / count as f64))
        .collect()
}

fn point_along(line: &[[f64; 2]], mut distance: f64) -> Option<([f64; 2], [f64; 2])> {
    for pair in line.windows(2) {
        let span = (pair[1][0] - pair[0][0]).hypot(pair[1][1] - pair[0][1]);
        if span <= 1.0e-9 {
            continue;
        }
        let tangent = [
            (pair[1][0] - pair[0][0]) / span,
            (pair[1][1] - pair[0][1]) / span,
        ];
        if distance <= span + 1.0e-9 {
            return Some((
                [
                    pair[0][0] + tangent[0] * distance,
                    pair[0][1] + tangent[1] * distance,
                ],
                tangent,
            ));
        }
        distance -= span;
    }
    None
}

fn mostly_within(line: &[[f64; 2]], spine: &[[f64; 2]], reach: f64) -> bool {
    let samples = resample(line, RAIL_STEP_M);
    let near = samples
        .iter()
        .filter(|(point, _)| {
            let nearest = nearest_point(spine, *point);
            (nearest[0] - point[0]).hypot(nearest[1] - point[1]) <= reach
        })
        .count();
    !samples.is_empty() && near * 2 >= samples.len()
}

fn nearest_point(line: &[[f64; 2]], point: [f64; 2]) -> [f64; 2] {
    let mut best = line[0];
    let mut best_distance = f64::INFINITY;
    for pair in line.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let span = [b[0] - a[0], b[1] - a[1]];
        let length_squared = span[0] * span[0] + span[1] * span[1];
        let t = if length_squared > 0.0 {
            (((point[0] - a[0]) * span[0] + (point[1] - a[1]) * span[1]) / length_squared)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let candidate = [a[0] + span[0] * t, a[1] + span[1] * t];
        let distance = (candidate[0] - point[0]).hypot(candidate[1] - point[1]);
        if distance < best_distance {
            best = candidate;
            best_distance = distance;
        }
    }
    best
}

fn clip_polyline(points: &[[f64; 2]], min: [f64; 2], max: [f64; 2]) -> Vec<Vec<[f64; 2]>> {
    let mut parts: Vec<Vec<[f64; 2]>> = Vec::new();
    for pair in points.windows(2) {
        let Some([a, b]) = super::clip_segment(
            pair[0].map(|value| value as f32),
            pair[1].map(|value| value as f32),
            min.map(|value| value as f32),
            max.map(|value| value as f32),
        ) else {
            continue;
        };
        let (a, b) = (a.map(f64::from), b.map(f64::from));
        if (b[0] - a[0]).hypot(b[1] - a[1]) < 0.1 {
            continue;
        }
        match parts.last_mut() {
            Some(part)
                if part
                    .last()
                    .is_some_and(|last| (last[0] - a[0]).hypot(last[1] - a[1]) < 0.01) =>
            {
                part.push(b);
            }
            _ => parts.push(vec![a, b]),
        }
    }
    parts
}

fn polygon(id: String, ring: &[[f64; 2]], projection: Projection, properties: Value) -> Value {
    let mut coordinates = without_collinear(ring)
        .iter()
        .map(|point| {
            let [latitude, longitude] = projection.unproject(point[0], point[1]);
            json!([longitude, latitude])
        })
        .collect::<Vec<_>>();
    coordinates.push(coordinates[0].clone());
    json!({"type": "Feature", "id": id, "properties": properties,
        "geometry": {"type": "Polygon", "coordinates": [coordinates]}})
}

/// Drops ring vertices within 5 cm of the line through their neighbours: a
/// straight run sampled every few metres otherwise yields slivers that
/// triangulate to zero area once vertices are stored as `f32`.
fn without_collinear(ring: &[[f64; 2]]) -> Vec<[f64; 2]> {
    const TOLERANCE_M: f64 = 0.05;
    let mut ring = ring.to_vec();
    let mut index = 0;
    while ring.len() > 3 && index < ring.len() {
        let previous = ring[(index + ring.len() - 1) % ring.len()];
        let current = ring[index];
        let next = ring[(index + 1) % ring.len()];
        let chord = [next[0] - previous[0], next[1] - previous[1]];
        let span = chord[0].hypot(chord[1]);
        let cross = chord[0] * (current[1] - previous[1]) - chord[1] * (current[0] - previous[0]);
        let near_previous =
            (current[0] - previous[0]).hypot(current[1] - previous[1]) < TOLERANCE_M;
        if near_previous || span < TOLERANCE_M || cross.abs() < TOLERANCE_M * span {
            ring.remove(index);
            index = index.saturating_sub(1);
        } else {
            index += 1;
        }
    }
    ring
}

fn length(line: &[[f64; 2]]) -> f64 {
    line.windows(2)
        .map(|pair| (pair[1][0] - pair[0][0]).hypot(pair[1][1] - pair[0][1]))
        .sum()
}

fn bounds(ring: &[[f64; 2]]) -> [[f64; 2]; 2] {
    ring.iter().fold(
        [[f64::INFINITY; 2], [f64::NEG_INFINITY; 2]],
        |[min, max], point| {
            [
                [min[0].min(point[0]), min[1].min(point[1])],
                [max[0].max(point[0]), max[1].max(point[1])],
            ]
        },
    )
}

fn cell(value: f64) -> i64 {
    (value / CELL_M).floor() as i64
}

fn inside([x, y]: [f64; 2], ring: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for index in 0..ring.len() {
        let [x1, y1] = ring[index];
        let [x2, y2] = ring[(index + 1) % ring.len()];
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    inside
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    }
}

fn round_dm(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTRE: [f64; 2] = [41.9656, -87.673];

    fn frame() -> Projection {
        Projection { center: CENTRE }
    }

    /// Rasters over ±250 m whose (surface, ground) heights come from a
    /// function of build-frame metres.
    fn rasters(heights: impl Fn([f64; 2]) -> (f64, f64)) -> Rasters {
        let [south, west] = frame().unproject(-250.0, -250.0);
        let [north, east] = frame().unproject(250.0, 250.0);
        Rasters::synthetic([south, west, north, east], |lon, lat| {
            heights(frame().project(lon, lat))
        })
    }

    fn ring_x(feature: &Value) -> [f64; 2] {
        feature["geometry"]["coordinates"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|point| frame().project(point[0].as_f64().unwrap(), point[1].as_f64().unwrap())[0])
            .fold([f64::INFINITY, f64::NEG_INFINITY], |[low, high], x| {
                [low.min(x), high.max(x)]
            })
    }

    #[test]
    fn fences_follow_the_alley_edge_with_gangway_gaps_and_skip_cars_and_garages() {
        let between = |value: f64, range: [f64; 2]| (range[0]..=range[1]).contains(&value);
        let rasters = rasters(|[x, y]| {
            let fence =
                between(y, [2.75, 3.25]) && between(x, [5.0, 20.0]) && !between(x, [12.0, 13.5]);
            let car = between(y, [-4.5, -2.7]) && between(x, [10.0, 15.0]);
            let surface = if fence {
                1.8
            } else if car {
                1.5
            } else {
                0.0
            };
            (surface + 180.0, 180.0)
        });
        let corner = |x: f64, y: f64| {
            let [lat, lon] = frame().unproject(x, y);
            json!([lon, lat])
        };
        let garage = json!({"features": [{"geometry": {"coordinates": [[
            corner(25.0, 2.5), corner(32.0, 2.5), corner(32.0, 9.0), corner(25.0, 9.0), corner(25.0, 2.5)]]}}]});
        let alley = Street {
            id: "way/9".into(),
            name: String::new(),
            highway: "service".into(),
            service: "alley".into(),
            points: vec![[0.0, 0.0], [40.0, 0.0]],
        };
        let footprints = Footprints::from_geojson(&garage, frame());
        let fences = fences(&[alley], &footprints, &rasters, frame());
        assert_eq!(fences.len(), 2, "{fences:#?}");
        for fence in &fences {
            assert_eq!(fence["properties"]["kind"], "fence");
            assert_eq!(fence["properties"]["height"], 1.8);
            assert!(fence["id"].as_str().unwrap().contains('l'));
        }
        let [first, second] = [ring_x(&fences[0]), ring_x(&fences[1])];
        assert!(
            (first[0] - 5.0).abs() < 0.8 && (first[1] - 12.0).abs() < 0.8,
            "{first:?}"
        );
        assert!(
            (second[0] - 13.5).abs() < 0.8 && (second[1] - 20.0).abs() < 0.8,
            "{second:?}"
        );
    }

    #[test]
    fn rail_builds_a_walled_embankment_an_open_underpass_and_per_track_elevated_strips() {
        let mut elements = Vec::new();
        let mut next = 0;
        let mut way = |tags: Value, points: &[[f64; 2]]| {
            let mut refs = Vec::new();
            for point in points {
                next += 1;
                let [lat, lon] = frame().unproject(point[0], point[1]);
                elements.push(json!({"type": "node", "id": next, "lat": lat, "lon": lon}));
                refs.push(next);
            }
            next += 1;
            elements.push(json!({"type": "way", "id": next, "nodes": refs, "tags": tags}));
        };
        let track = json!({"railway": "rail"});
        let bridge = json!({"railway": "rail", "bridge": "yes", "layer": "1"});
        let wall = json!({"barrier": "retaining_wall"});
        let elevated = json!({"railway": "subway", "bridge": "yes", "layer": "2"});
        way(track.clone(), &[[0.0, -100.0], [0.0, 100.0]]);
        way(track, &[[-5.0, 100.0], [-5.0, -100.0]]);
        way(wall.clone(), &[[-15.0, -100.0], [-15.0, 100.0]]);
        way(wall, &[[10.0, -100.0], [10.0, 100.0]]);
        way(bridge, &[[0.0, 100.0], [0.0, 128.0]]);
        way(elevated.clone(), &[[50.0, -100.0], [50.0, 100.0]]);
        way(elevated, &[[54.0, 100.0], [54.0, -100.0]]);
        let raw = json!({"elements": elements});
        let rasters = rasters(|[x, y]| {
            let fill = (-15.0..=10.0).contains(&x) && (-100.0..=100.0).contains(&y);
            let deck = (-15.0..=10.0).contains(&x) && (100.5..=128.0).contains(&y);
            let elevated = (48.0..=56.0).contains(&x) && (-100.0..=100.0).contains(&y);
            let ground = if fill { 3.8 } else { 0.0 };
            let surface = ground
                + if deck {
                    4.8
                } else if elevated {
                    5.5
                } else {
                    0.0
                };
            (surface, ground)
        });
        let (features, summary) = rail(
            &raw,
            frame(),
            [[-200.0, -200.0], [200.0, 200.0]],
            Some(&rasters),
        );
        let kinds = |kind: &str| {
            features
                .iter()
                .filter(|feature| feature["properties"]["kind"] == kind)
                .collect::<Vec<_>>()
        };
        let embankment = kinds("rail_embankment");
        assert_eq!(embankment.len(), 1, "{summary}");
        assert_eq!(embankment[0]["properties"]["height"], 3.8);
        assert_eq!(
            embankment[0]["properties"]["roof_material"],
            BALLAST_MATERIAL
        );
        let [west, east] = ring_x(embankment[0]);
        assert!(
            (west + 15.0).abs() < 0.5 && (east - 10.0).abs() < 0.5,
            "{west} {east}"
        );
        let decks = kinds("rail_deck");
        assert_eq!(decks.len(), 3, "{summary}");
        let underpass = decks
            .iter()
            .find(|deck| deck["properties"]["material"] == BRIDGE_DECK_MATERIAL)
            .unwrap();
        assert_eq!(underpass["properties"]["height"], 4.8);
        assert_eq!(underpass["properties"]["min_height"], 3.8);
        let strips = decks
            .iter()
            .filter(|deck| deck["properties"]["material"] == ELEVATED_DECK_MATERIAL)
            .map(|deck| (ring_x(deck), deck["properties"]["height"].as_f64().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(strips.len(), 2);
        // Straight 200 m strips keep only their corners: no collinear slivers.
        for deck in &decks {
            assert_eq!(
                deck["geometry"]["coordinates"][0].as_array().unwrap().len(),
                5,
                "{deck}"
            );
        }
        for ([low, high], top) in strips {
            assert!((high - low - 2.0 * ELEVATED_HALF_WIDTH_M).abs() < 0.1 && top == 5.5);
        }
        // No LiDAR: walls alone mark the embankment at the default height.
        let (features, _) = rail(&raw, frame(), [[-200.0, -200.0], [200.0, 200.0]], None);
        assert!(
            features
                .iter()
                .any(|feature| feature["properties"]["height"] == DEFAULT_EMBANKMENT_M)
        );
    }
}
