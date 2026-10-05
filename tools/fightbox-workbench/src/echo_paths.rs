//! Run-time geometric planner for the discrete late echoes of one static,
//! impulsive source.
//!
//! Baked pathing already renders the shot's primary street-route arrival, so a
//! bare route is never a tap. Echoes are only transport that arrives distinctly
//! after that primary:
//! - a routed reflection: the shot's street route (or the source itself) to an
//!   entry point, one finite facade patch, then the listener;
//! - an alternate street route whose last corner differs from the primary's.
//!
//! Routes are shortest paths over an ear-height visibility graph of convex
//! vertical building corners, built once per static source position. The
//! primary route also times the source's baked pathing as the listener moves.
//! All of this runs on the control thread; the backend only freezes the result.

use std::collections::BTreeMap;

use fightbox_api::EnuVector3;
use fightbox_runtime::{FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M, MAX_ACTIVE_SOURCES};
use fightbox_steam_audio::{
    CORNER_LOSS_DB_HIGH, CORNER_LOSS_DB_LOW, CORNER_LOSS_DB_MID, EchoPathGeometry, EchoPathKind,
    EchoPlanPublisher, EchoPrimary, EchoTrigger, EchoTriggerControl, PrimaryRoute,
};
use fightbox_world::{AcousticMesh, EchoExtractor, EchoExtractorConfig, MaterialTable};

pub(crate) const SPEED_OF_SOUND_MPS: f64 = 343.0;
/// Echo window after the primary arrival. The lower bound admits a local
/// transverse canyon return (22.9 m street: ~133 ms round trip).
pub(crate) const MIN_EXCESS_S: f64 = 0.050;
pub(crate) const MAX_EXCESS_S: f64 = 1.200;
pub(crate) const MAX_TAPS: usize = 4;
/// Two taps closer than this are one perceived arrival; keep the stronger.
const MIN_TAP_SEPARATION_S: f64 = 0.020;
/// The backend echo delay line spans this much propagation.
const MAX_RENDER_DELAY_PATH_M: f64 = 2_048.0;
const RAY_EPSILON_M: f64 = 0.01;
/// Route vertices sit this far outside their corner, off both faces.
const CORNER_OFFSET_M: f64 = 0.1;
/// Minimum distance in front of a reflector plane; also keeps a corner vertex
/// from reflecting off its own faces.
const FACING_EPSILON_M: f64 = 0.25;
const PATCH_TOLERANCE_M: f64 = 0.02;
/// A route vertex turning less than this is a string pulled past a jittered
/// facade, not a street corner, and carries no corner loss.
const MIN_CORNER_TURN_DEG: f64 = 30.0;
/// Listener movement that retimes routed pathing; the backend's one-pole
/// smooths the steps.
const ROUTE_FOLLOW_M: f64 = 0.02;
/// Source movement that rebuilds its street routes.
const ROUTE_FIELD_REBUILD_M: f64 = 0.5;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct V3 {
    x: f64,
    y: f64,
    z: f64,
}

impl V3 {
    const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    fn from_enu(value: EnuVector3) -> Self {
        Self::new(
            f64::from(value.east_m),
            f64::from(value.north_m),
            f64::from(value.up_m),
        )
    }

    fn from_f32(value: [f32; 3]) -> Self {
        Self::new(
            f64::from(value[0]),
            f64::from(value[1]),
            f64::from(value[2]),
        )
    }

    fn enu(self) -> EnuVector3 {
        EnuVector3::new(self.x as f32, self.y as f32, self.z as f32)
    }

    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }

    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }

    fn scale(self, factor: f64) -> Self {
        Self::new(self.x * factor, self.y * factor, self.z * factor)
    }

    fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    fn cross(self, other: Self) -> Self {
        Self::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    fn horizontal(self) -> Self {
        Self::new(self.x, self.y, 0.0)
    }

    fn normalized(self) -> Option<Self> {
        let length = self.length();
        (length.is_finite() && length > 1.0e-9).then(|| self.scale(1.0 / length))
    }
}

/// Signed area of the horizontal turn from `a` to `b`; positive turns left.
fn turn(a: V3, b: V3) -> f64 {
    a.x * b.y - a.y * b.x
}

/// Horizontal heading change at `vertex` between the legs from `from` and
/// toward `to`.
fn turn_degrees(from: V3, vertex: V3, to: V3) -> f64 {
    let incoming = vertex.sub(from).horizontal();
    let outgoing = to.sub(vertex).horizontal();
    let scale = incoming.length() * outgoing.length();
    if scale <= 0.0 {
        return 0.0;
    }
    (incoming.dot(outgoing) / scale)
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

fn stable_hash(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

/// Ray-versus-mesh visibility over a uniform horizontal grid.
struct Occluder {
    triangles: Vec<[V3; 3]>,
    z_ranges: Vec<[f64; 2]>,
    origin: [f64; 2],
    cell_m: f64,
    columns: usize,
    rows: usize,
    cells: Vec<Vec<u32>>,
}

impl Occluder {
    fn new(mesh: &AcousticMesh) -> Self {
        let triangles = mesh
            .triangles
            .iter()
            .map(|triangle| triangle.map(|index| V3::from_enu(mesh.vertices_enu_m[index as usize])))
            .collect::<Vec<_>>();
        let z_ranges = triangles
            .iter()
            .map(|vertices| {
                vertices
                    .iter()
                    .fold([f64::INFINITY, f64::NEG_INFINITY], |range, vertex| {
                        [range[0].min(vertex.z), range[1].max(vertex.z)]
                    })
            })
            .collect();
        let (mut min, mut max) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for vertex in triangles.iter().flatten() {
            min = [min[0].min(vertex.x), min[1].min(vertex.y)];
            max = [max[0].max(vertex.x), max[1].max(vertex.y)];
        }
        if !min[0].is_finite() {
            (min, max) = ([0.0; 2], [1.0; 2]);
        }
        let origin = [min[0] - 1.0, min[1] - 1.0];
        let span = [max[0] + 1.0 - origin[0], max[1] + 1.0 - origin[1]];
        let cell_m = (span[0].max(span[1]) / 128.0).max(4.0);
        let columns = (span[0] / cell_m).ceil() as usize + 1;
        let rows = (span[1] / cell_m).ceil() as usize + 1;
        let mut occluder = Self {
            triangles,
            z_ranges,
            origin,
            cell_m,
            columns,
            rows,
            cells: vec![Vec::new(); columns * rows],
        };
        for index in 0..occluder.triangles.len() {
            let vertices = occluder.triangles[index];
            let low = occluder.cell_of(
                vertices.iter().map(|v| v.x).fold(f64::INFINITY, f64::min),
                vertices.iter().map(|v| v.y).fold(f64::INFINITY, f64::min),
            );
            let high = occluder.cell_of(
                vertices
                    .iter()
                    .map(|v| v.x)
                    .fold(f64::NEG_INFINITY, f64::max),
                vertices
                    .iter()
                    .map(|v| v.y)
                    .fold(f64::NEG_INFINITY, f64::max),
            );
            for row in low.1..=high.1 {
                for column in low.0..=high.0 {
                    occluder.cells[row * columns + column].push(index as u32);
                }
            }
        }
        occluder
    }

    fn cell_of(&self, x: f64, y: f64) -> (usize, usize) {
        let column = ((x - self.origin[0]) / self.cell_m).floor();
        let row = ((y - self.origin[1]) / self.cell_m).floor();
        (
            column.clamp(0.0, (self.columns - 1) as f64) as usize,
            row.clamp(0.0, (self.rows - 1) as f64) as usize,
        )
    }

    /// True when no triangle outside `exclude` (sorted) crosses the open
    /// segment, ignoring hits within the ray epsilon of either endpoint.
    fn clear(&self, from: V3, to: V3, exclude: &[u32]) -> bool {
        let length = to.sub(from).length();
        if length <= 2.0 * RAY_EPSILON_M {
            return true;
        }
        let z_low = from.z.min(to.z) - RAY_EPSILON_M;
        let z_high = from.z.max(to.z) + RAY_EPSILON_M;
        let mut clear = true;
        self.walk(from, to, |cell| {
            for &triangle in &self.cells[cell] {
                let [bottom, top] = self.z_ranges[triangle as usize];
                if top < z_low || bottom > z_high || exclude.binary_search(&triangle).is_ok() {
                    continue;
                }
                if let Some(fraction) =
                    segment_triangle(from, to, self.triangles[triangle as usize])
                    && fraction * length > RAY_EPSILON_M
                    && (1.0 - fraction) * length > RAY_EPSILON_M
                {
                    clear = false;
                    return false;
                }
            }
            true
        });
        clear
    }

    /// Visits the grid cells a segment crosses, in order (Amanatides-Woo).
    fn walk(&self, from: V3, to: V3, mut visit: impl FnMut(usize) -> bool) {
        let (mut column, mut row) = self.cell_of(from.x, from.y);
        let delta = to.sub(from);
        let axis = |start: f64, delta: f64, cell: usize, origin: f64| {
            if delta == 0.0 {
                return (0_isize, f64::INFINITY, f64::INFINITY);
            }
            let step = if delta > 0.0 { 1 } else { -1 };
            let boundary = origin + (cell as f64 + f64::from(u8::from(delta > 0.0))) * self.cell_m;
            (step, (boundary - start) / delta, self.cell_m / delta.abs())
        };
        let (step_x, mut next_x, delta_x) = axis(from.x, delta.x, column, self.origin[0]);
        let (step_y, mut next_y, delta_y) = axis(from.y, delta.y, row, self.origin[1]);
        loop {
            if !visit(row * self.columns + column) {
                return;
            }
            if next_x.min(next_y) > 1.0 {
                return;
            }
            if next_x < next_y {
                let Some(next) = column
                    .checked_add_signed(step_x)
                    .filter(|c| *c < self.columns)
                else {
                    return;
                };
                column = next;
                next_x += delta_x;
            } else {
                let Some(next) = row.checked_add_signed(step_y).filter(|r| *r < self.rows) else {
                    return;
                };
                row = next;
                next_y += delta_y;
            }
        }
    }
}

/// Möller-Trumbore on a segment; the hit's fraction along `from -> to`.
fn segment_triangle(from: V3, to: V3, vertices: [V3; 3]) -> Option<f64> {
    let direction = to.sub(from);
    let edge_a = vertices[1].sub(vertices[0]);
    let edge_b = vertices[2].sub(vertices[0]);
    let p = direction.cross(edge_b);
    let determinant = edge_a.dot(p);
    if determinant.abs() <= 1.0e-12 * direction.length() * edge_a.length() * edge_b.length() {
        return None;
    }
    let inverse = 1.0 / determinant;
    let s = from.sub(vertices[0]);
    let u = s.dot(p) * inverse;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(edge_a);
    let v = direction.dot(q) * inverse;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let fraction = edge_b.dot(q) * inverse;
    (0.0..=1.0).contains(&fraction).then_some(fraction)
}

fn point_in_triangle(point: V3, vertices: [V3; 3]) -> bool {
    let edge_a = vertices[1].sub(vertices[0]);
    let edge_b = vertices[2].sub(vertices[0]);
    let normal = edge_a.cross(edge_b);
    let Some(unit) = normal.normalized() else {
        return false;
    };
    let offset = point.sub(vertices[0]);
    if offset.dot(unit).abs() > PATCH_TOLERANCE_M {
        return false;
    }
    // Signed distance inside every edge, widened by the patch tolerance.
    [
        (vertices[0], vertices[1]),
        (vertices[1], vertices[2]),
        (vertices[2], vertices[0]),
    ]
    .iter()
    .all(|(start, end)| {
        let edge = end.sub(*start);
        edge.cross(point.sub(*start)).dot(unit) >= -PATCH_TOLERANCE_M * edge.length()
    })
}

struct Reflector {
    id: u32,
    normal: V3,
    offset: f64,
    centroid: V3,
    /// Sorted mesh triangle indices; excluded from this patch's own legs.
    triangles: Vec<u32>,
    pressure: [f64; 3],
}

struct RouteCorner {
    id: u32,
    position: V3,
    outward: V3,
    normals: [V3; 2],
    z_range: [f64; 2],
}

impl RouteCorner {
    /// A line through a convex corner leaves the building on one side only
    /// when it does not point into (or straight away from) the wedge.
    fn tangent(&self, direction: V3) -> bool {
        direction.dot(self.normals[0]) * direction.dot(self.normals[1]) <= 0.0
    }

    /// A string pulled from `from` through this corner to `to` wraps it.
    fn taut(&self, vertex: V3, from: V3, to: V3) -> bool {
        let incoming = vertex.sub(from).horizontal();
        let outgoing = to.sub(vertex).horizontal();
        let bend = turn(incoming, outgoing);
        self.tangent(outgoing)
            && bend * turn(incoming, self.outward.scale(-1.0)) > 0.0
            && bend.abs() > 1.0e-9 * incoming.length() * outgoing.length()
    }
}

/// Scene geometry for echo planning, extracted once from the acoustic mesh.
pub(crate) struct EchoPathPlanner {
    occluder: Occluder,
    reflectors: Vec<Reflector>,
    corners: Vec<RouteCorner>,
    air_exponents: [f32; 3],
}

/// Shortest street routes from one static source to every reachable corner.
pub(crate) struct RouteField {
    source: V3,
    nodes: Vec<(usize, V3)>,
    distance: Vec<f64>,
    /// `None` means the route comes straight from the source.
    parent: Vec<Option<usize>>,
}

impl RouteField {
    fn position(&self, node: Option<usize>) -> V3 {
        node.map_or(self.source, |node| self.nodes[node].1)
    }

    fn length(&self, node: Option<usize>) -> f64 {
        node.map_or(0.0, |node| self.distance[node])
    }

    fn chain(&self, node: Option<usize>) -> Vec<usize> {
        let mut chain = Vec::new();
        let mut current = node;
        while let Some(index) = current {
            chain.push(index);
            current = self.parent[index];
        }
        chain
    }

    fn polyline(&self, node: Option<usize>, ending: &[V3]) -> Vec<EnuVector3> {
        let chain = self.chain(node);
        let mut points = Vec::with_capacity(1 + chain.len() + ending.len());
        points.push(self.source.enu());
        points.extend(
            chain
                .into_iter()
                .rev()
                .map(|index| self.nodes[index].1.enu()),
        );
        points.extend(ending.iter().map(|point| point.enu()));
        points
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EchoRoute {
    /// Street route to an entry, one facade bounce, then the listener.
    RoutedReflection { entry_is_source: bool },
    /// A street route whose last corner differs from the primary's.
    AlternateRoute,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlannedPath {
    pub(crate) route: EchoRoute,
    pub(crate) geometry: EchoPathGeometry,
    pub(crate) polyline_enu_m: Vec<EnuVector3>,
    pub(crate) facade_id: Option<u32>,
    /// Physical arrival after the primary street route.
    pub(crate) excess_s: f64,
    /// Route corners not shared with the primary that turn at least
    /// [`MIN_CORNER_TURN_DEG`], each voiced as a corner.
    pub(crate) charged_corners: usize,
    /// Mean band pressure over the rendered primary's.
    pub(crate) predicted_pressure: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EchoPathPlan {
    pub(crate) straight_line_m: f64,
    /// Shortest street route; `None` when no route reaches the listener.
    pub(crate) primary_route_m: Option<f64>,
    /// The route timing baked pathing: a routed primary within the delay
    /// horizon. Tap render delays follow it.
    pub(crate) timed_route: Option<PrimaryRoute>,
    pub(crate) line_of_sight: bool,
    /// Where the primary reaches the listener from: its last corner, or the
    /// source itself in line of sight.
    pub(crate) primary_arrival: EnuVector3,
    pub(crate) primary_polyline_enu_m: Vec<EnuVector3>,
    /// Route vertices of the primary, and those turning a real corner.
    pub(crate) primary_corners: usize,
    pub(crate) primary_turns: usize,
    /// Valid, in-window candidates before selection.
    pub(crate) candidates: usize,
    pub(crate) taps: Vec<PlannedPath>,
}

impl EchoPathPlan {
    pub(crate) fn geometry(&self) -> Vec<EchoPathGeometry> {
        self.taps.iter().map(|tap| tap.geometry).collect()
    }

    /// The rendered primary the taps trail.
    pub(crate) fn primary(&self) -> EchoPrimary {
        match self.primary_route_m {
            Some(length) if !self.line_of_sight => EchoPrimary::Routed {
                path_length_m: length as f32,
            },
            _ => EchoPrimary::LineOfSight,
        }
    }

    pub(crate) fn summary(&self) -> String {
        let mut text = format!(
            "straight {:.1} m, primary route {}, {} vertices ({} turn(s) >= {MIN_CORNER_TURN_DEG} deg){}, arriving from [{:.1}, {:.1}]; {} in-window candidate(s), {} tap(s)",
            self.straight_line_m,
            self.primary_route_m
                .map_or("none".to_owned(), |length| format!("{length:.1} m")),
            self.primary_corners,
            self.primary_turns,
            if self.line_of_sight {
                ", line of sight"
            } else {
                ""
            },
            self.primary_arrival.east_m,
            self.primary_arrival.north_m,
            self.candidates,
            self.taps.len()
        );
        for tap in &self.taps {
            let arrival = tap.geometry.arrival_position_enu;
            text.push_str(&format!(
                "\n  {:?} id={:#010x}: path {:.1} m, +{:.0} ms after primary, render delay path {:.1} m, arrival [{:.1}, {:.1}, {:.1}], band pressure [{:.3}, {:.3}, {:.3}], charged corners {}, predicted {:.2e}",
                tap.route,
                tap.geometry.stable_path_id,
                tap.geometry.physical_path_length_m,
                tap.excess_s * 1_000.0,
                tap.geometry.render_delay_path_m,
                arrival.east_m,
                arrival.north_m,
                arrival.up_m,
                tap.geometry.band_pressure_gain[0],
                tap.geometry.band_pressure_gain[1],
                tap.geometry.band_pressure_gain[2],
                tap.charged_corners,
                tap.predicted_pressure,
            ));
        }
        text
    }
}

fn corner_pressure() -> [f64; 3] {
    [CORNER_LOSS_DB_LOW, CORNER_LOSS_DB_MID, CORNER_LOSS_DB_HIGH]
        .map(|loss_db| 10.0_f64.powf(f64::from(loss_db) / 20.0))
}

impl EchoPathPlanner {
    pub(crate) fn new(mesh: &AcousticMesh, materials: &MaterialTable) -> Result<Self, String> {
        let extractor = EchoExtractor::new(EchoExtractorConfig::default())
            .map_err(|error| format!("echo extractor: {error}"))?;
        let (patches, edges) = extractor
            .facade_geometry(mesh, materials)
            .map_err(|error| format!("echo facade extraction: {error}"))?;
        let material_list = materials
            .iter()
            .map(|(_, material)| material)
            .collect::<Vec<_>>();
        let mut reflectors = Vec::with_capacity(patches.len());
        let mut by_key = BTreeMap::new();
        for patch in &patches {
            let Some(normal) = V3::from_f32(patch.normal_city_enu).normalized() else {
                continue;
            };
            let vertices = patch
                .boundary_vertices_city_enu_m
                .iter()
                .map(|vertex| V3::from_f32(*vertex))
                .collect::<Vec<_>>();
            if vertices.is_empty() {
                continue;
            }
            let centroid = vertices
                .iter()
                .fold(V3::default(), |sum, vertex| sum.add(*vertex))
                .scale(1.0 / vertices.len() as f64);
            let material = material_list
                .get(patch.material_id as usize)
                .ok_or("facade material is outside the material table")?;
            let scattering = (1.0 - f64::from(material.scattering)).sqrt();
            let mut triangles = patch.triangle_indices.clone();
            triangles.sort_unstable();
            by_key.insert(patch.key, reflectors.len());
            reflectors.push(Reflector {
                id: stable_hash(&patch.key.0) & 0x7fff_ffff,
                normal,
                offset: vertices
                    .iter()
                    .map(|vertex| normal.dot(*vertex))
                    .sum::<f64>()
                    / vertices.len() as f64,
                centroid,
                triangles,
                pressure: material
                    .absorption
                    .map(|absorption| (1.0 - f64::from(absorption)).sqrt() * scattering),
            });
        }
        let mut corners = Vec::new();
        for edge in &edges {
            let [Some(first), Some(second)] = [0, 1].map(|slot| {
                edge.adjacent_patches
                    .get(slot)
                    .and_then(|key| by_key.get(key).copied())
            }) else {
                // Open wall ends are not route vertices.
                continue;
            };
            let base = V3::from_f32(edge.endpoints_city_enu_m[0]);
            let (a, b) = (&reflectors[first], &reflectors[second]);
            let (Some(normal_a), Some(normal_b)) = (
                a.normal.horizontal().normalized(),
                b.normal.horizontal().normalized(),
            ) else {
                continue;
            };
            // Convex only: each face recedes behind the other's plane.
            let convex = b.centroid.sub(base).horizontal().dot(normal_a) < -PATCH_TOLERANCE_M
                && a.centroid.sub(base).horizontal().dot(normal_b) < -PATCH_TOLERANCE_M;
            let Some(outward) = normal_a.add(normal_b).normalized() else {
                continue;
            };
            if !convex {
                continue;
            }
            let [low, high] = edge
                .endpoints_city_enu_m
                .map(|endpoint| f64::from(endpoint[2]));
            corners.push(RouteCorner {
                id: stable_hash(&edge.key.0) & 0x7fff_ffff,
                position: base.horizontal().add(outward.scale(CORNER_OFFSET_M)),
                outward,
                normals: [normal_a, normal_b],
                z_range: [low.min(high), low.max(high)],
            });
        }
        Ok(Self {
            occluder: Occluder::new(mesh),
            reflectors,
            corners,
            air_exponents: FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M,
        })
    }

    pub(crate) fn set_air_exponents(&mut self, air_exponents: [f32; 3]) {
        self.air_exponents = air_exponents;
    }

    #[cfg(test)]
    fn reflector_count(&self) -> usize {
        self.reflectors.len()
    }

    #[cfg(test)]
    fn corner_count(&self) -> usize {
        self.corners.len()
    }

    /// Street routes at the source's height; cost grows with corner pairs,
    /// so build it once per static source position, not per trigger.
    pub(crate) fn route_field(&self, source: EnuVector3) -> RouteField {
        let source = V3::from_enu(source);
        let nodes = self
            .corners
            .iter()
            .enumerate()
            .filter(|(_, corner)| (corner.z_range[0]..=corner.z_range[1]).contains(&source.z))
            .map(|(index, corner)| {
                (
                    index,
                    V3::new(corner.position.x, corner.position.y, source.z),
                )
            })
            .collect::<Vec<_>>();
        let mut adjacency = vec![Vec::new(); nodes.len()];
        for first in 0..nodes.len() {
            for second in first + 1..nodes.len() {
                let (corner_a, a) = nodes[first];
                let (corner_b, b) = nodes[second];
                let direction = b.sub(a);
                if self.corners[corner_a].tangent(direction)
                    && self.corners[corner_b].tangent(direction)
                    && self.occluder.clear(a, b, &[])
                {
                    let length = direction.length();
                    adjacency[first].push((second, length));
                    adjacency[second].push((first, length));
                }
            }
        }
        let mut distance = nodes
            .iter()
            .map(|(corner, position)| {
                let direction = position.sub(source);
                if self.corners[*corner].tangent(direction)
                    && self.occluder.clear(source, *position, &[])
                {
                    direction.length()
                } else {
                    f64::INFINITY
                }
            })
            .collect::<Vec<_>>();
        let mut parent = vec![None; nodes.len()];
        let mut settled = vec![false; nodes.len()];
        while let Some(current) = (0..nodes.len())
            .filter(|index| !settled[*index] && distance[*index].is_finite())
            .min_by(|a, b| distance[*a].total_cmp(&distance[*b]).then(a.cmp(b)))
        {
            settled[current] = true;
            for &(next, length) in &adjacency[current] {
                let through = distance[current] + length;
                if !settled[next] && through < distance[next] {
                    distance[next] = through;
                    parent[next] = Some(current);
                }
            }
        }
        RouteField {
            source,
            nodes,
            distance,
            parent,
        }
    }

    fn sees_listener(&self, field: &RouteField, listener: V3) -> Vec<bool> {
        field
            .nodes
            .iter()
            .enumerate()
            .map(|(node, (corner, position))| {
                field.distance[node].is_finite()
                    && self.corners[*corner].tangent(listener.sub(*position))
                    && self.occluder.clear(*position, listener, &[])
            })
            .collect()
    }

    /// Shortest street route's length and last node, out of line of sight.
    fn routed_primary(
        field: &RouteField,
        listener: V3,
        sees_listener: &[bool],
    ) -> Option<(f64, usize)> {
        (0..field.nodes.len())
            .filter(|node| sees_listener[*node])
            .map(|node| {
                (
                    field.distance[node] + listener.sub(field.nodes[node].1).length(),
                    node,
                )
            })
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
    }

    /// A routed primary as the backend times it; the topology names the
    /// route's corners, whichever field solved it.
    fn timing_route(&self, field: &RouteField, length_m: f64, last: usize) -> Option<PrimaryRoute> {
        let chain = field.chain(Some(last));
        let corners = chain
            .iter()
            .flat_map(|node| self.corners[field.nodes[*node].0].id.to_le_bytes())
            .collect::<Vec<_>>();
        (length_m <= MAX_RENDER_DELAY_PATH_M).then(|| PrimaryRoute {
            length_m: length_m as f32,
            topology_id: ((chain.len() as u64) << 32) | u64::from(stable_hash(&corners)),
        })
    }

    /// [`EchoPathPlan::timed_route`] alone, cheap enough to follow the listener.
    pub(crate) fn timed_route(
        &self,
        field: &RouteField,
        listener: EnuVector3,
    ) -> Option<PrimaryRoute> {
        let listener = V3::from_enu(listener);
        if self.occluder.clear(field.source, listener, &[]) {
            return None;
        }
        let sees_listener = self.sees_listener(field, listener);
        let (length_m, last) = Self::routed_primary(field, listener, &sees_listener)?;
        self.timing_route(field, length_m, last)
    }

    /// The street route this planner hears from `field`'s source to `point`:
    /// straight in line of sight, otherwise the shortest corner chain that
    /// sees it (the map's music field). `None` when no route reaches it.
    pub(crate) fn route_to(&self, field: &RouteField, point: EnuVector3) -> Option<Vec<EnuVector3>> {
        let target = V3::from_enu(point);
        if self.occluder.clear(field.source, target, &[]) {
            return Some(vec![field.source.enu(), point]);
        }
        let mut order = (0..field.nodes.len())
            .filter(|node| field.distance[*node].is_finite())
            .map(|node| (field.distance[node] + target.sub(field.nodes[node].1).length(), node))
            .collect::<Vec<_>>();
        order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        // Cheapest first; the first corner that sees the point is the route.
        order.into_iter().take(64).find_map(|(_, node)| {
            let (corner, position) = field.nodes[node];
            (self.corners[corner].tangent(target.sub(position))
                && self.occluder.clear(position, target, &[]))
            .then(|| field.polyline(Some(node), &[target]))
        })
    }

    /// Plans at most [`MAX_TAPS`] echoes of `field`'s source at `listener`.
    pub(crate) fn plan(&self, field: &RouteField, listener: EnuVector3) -> EchoPathPlan {
        let listener = V3::from_enu(listener);
        let source = field.source;
        let straight_line_m = listener.sub(source).length();
        let line_of_sight = self.occluder.clear(source, listener, &[]);
        let sees_listener = self.sees_listener(field, listener);
        let primary = if line_of_sight {
            Some((straight_line_m, None))
        } else {
            Self::routed_primary(field, listener, &sees_listener)
                .map(|(length_m, last)| (length_m, Some(last)))
        };
        let Some((primary_m, primary_last)) = primary else {
            return EchoPathPlan {
                straight_line_m,
                primary_route_m: None,
                timed_route: None,
                line_of_sight,
                primary_arrival: source.enu(),
                primary_polyline_enu_m: Vec::new(),
                primary_corners: 0,
                primary_turns: 0,
                candidates: 0,
                taps: Vec::new(),
            };
        };
        let primary_chain = field.chain(primary_last);
        // Real corners on the route to `node`, then on toward `toward`; the
        // primary's own are skipped for a candidate, whose shared transport
        // the rendered primary already carries.
        let corners_turned = |node: Option<usize>, toward: V3, skip_primary: bool| {
            let mut next = toward;
            let mut count = 0;
            for index in field.chain(node) {
                let vertex = field.nodes[index].1;
                let turned = turn_degrees(field.position(field.parent[index]), vertex, next)
                    >= MIN_CORNER_TURN_DEG;
                if turned && !(skip_primary && primary_chain.contains(&index)) {
                    count += 1;
                }
                next = vertex;
            }
            count
        };
        let primary_turns = corners_turned(primary_last, listener, false);
        let corner = corner_pressure();
        let window = primary_m + MIN_EXCESS_S * SPEED_OF_SOUND_MPS
            ..=primary_m + MAX_EXCESS_S * SPEED_OF_SOUND_MPS;
        // Taps trail the primary as rendered: on its timed route, else on the
        // straight line that untimed pathing shares with direct.
        let timed_route = primary_last.and_then(|last| self.timing_route(field, primary_m, last));
        let rendered_primary_m = if timed_route.is_some() {
            primary_m
        } else {
            straight_line_m
        };
        let delay_fits =
            |length: f64| rendered_primary_m + (length - primary_m) <= MAX_RENDER_DELAY_PATH_M;
        let taut_at = |node: Option<usize>, toward: V3| {
            node.is_none_or(|node| {
                let (corner_index, position) = field.nodes[node];
                self.corners[corner_index].taut(
                    position,
                    field.position(field.parent[node]),
                    toward,
                )
            })
        };
        let mut candidates = Vec::new();
        let mut push = |route: EchoRoute,
                        kind: EchoPathKind,
                        id: u32,
                        length: f64,
                        arrival: V3,
                        interaction: [f64; 3],
                        extra: usize,
                        polyline_enu_m: Vec<EnuVector3>,
                        facade_id: Option<u32>| {
            // The backend voices a diffraction tap's last corner itself.
            let voiced = match kind {
                EchoPathKind::Specular => [1.0; 3],
                EchoPathKind::Diffraction => corner,
            };
            // Ranked as rendered: over the primary, so only the extra
            // spreading and the backend's shared air law count.
            let predicted = (0..3)
                .map(|band| {
                    let air_per_m = f64::from(self.air_exponents[band]);
                    interaction[band] * voiced[band] * (-air_per_m * (length - primary_m)).exp()
                })
                .sum::<f64>()
                * primary_m.max(1.0)
                / (3.0 * length.max(1.0));
            candidates.push(PlannedPath {
                route,
                geometry: EchoPathGeometry {
                    kind,
                    stable_path_id: id,
                    physical_path_length_m: length as f32,
                    render_delay_path_m: (rendered_primary_m + (length - primary_m)) as f32,
                    arrival_position_enu: arrival.enu(),
                    band_pressure_gain: interaction.map(|gain| gain as f32),
                },
                polyline_enu_m,
                facade_id,
                excess_s: (length - primary_m) / SPEED_OF_SOUND_MPS,
                charged_corners: extra,
                predicted_pressure: predicted,
            });
        };

        for (node, &(corner_index, position)) in field.nodes.iter().enumerate() {
            if !sees_listener[node] || Some(node) == primary_last {
                continue;
            }
            let length = field.distance[node] + listener.sub(position).length();
            if !window.contains(&length) || !delay_fits(length) || !taut_at(Some(node), listener) {
                continue;
            }
            let extra = corners_turned(Some(node), listener, true);
            // The backend voices one corner of a diffraction tap; a route
            // whose own vertices are all gentle bends goes unvoiced.
            let (kind, interaction) = match extra {
                0 => (EchoPathKind::Specular, [1.0; 3]),
                _ => (
                    EchoPathKind::Diffraction,
                    corner.map(|pressure| pressure.powi(extra as i32 - 1)),
                ),
            };
            push(
                EchoRoute::AlternateRoute,
                kind,
                0x8000_0000 | self.corners[corner_index].id,
                length,
                position,
                interaction,
                extra,
                field.polyline(Some(node), &[listener]),
                None,
            );
        }

        let entries = std::iter::once(None).chain(
            (0..field.nodes.len())
                .filter(|node| field.distance[*node].is_finite())
                .map(Some),
        );
        for entry in entries {
            let from = field.position(entry);
            let route_m = field.length(entry);
            let entry_id = entry.map_or(0, |node| self.corners[field.nodes[node].0].id);
            for reflector in &self.reflectors {
                let entry_height = reflector.normal.dot(from) - reflector.offset;
                let listener_height = reflector.normal.dot(listener) - reflector.offset;
                if entry_height <= FACING_EPSILON_M || listener_height <= FACING_EPSILON_M {
                    continue;
                }
                let image = from.sub(reflector.normal.scale(2.0 * entry_height));
                let length = route_m + listener.sub(image).length();
                if !window.contains(&length) || !delay_fits(length) {
                    continue;
                }
                let bounce = image.add(
                    listener
                        .sub(image)
                        .scale(entry_height / (entry_height + listener_height)),
                );
                if !taut_at(entry, bounce)
                    || !reflector.triangles.iter().any(|triangle| {
                        point_in_triangle(bounce, self.occluder.triangles[*triangle as usize])
                    })
                    || !self.occluder.clear(from, bounce, &reflector.triangles)
                    || !self.occluder.clear(bounce, listener, &reflector.triangles)
                {
                    continue;
                }
                let extra = corners_turned(entry, bounce, true);
                let mut bytes = [0_u8; 8];
                bytes[..4].copy_from_slice(&entry_id.to_le_bytes());
                bytes[4..].copy_from_slice(&reflector.id.to_le_bytes());
                push(
                    EchoRoute::RoutedReflection {
                        entry_is_source: entry.is_none(),
                    },
                    EchoPathKind::Specular,
                    stable_hash(&bytes) & 0x7fff_ffff,
                    length,
                    bounce,
                    std::array::from_fn(|band| {
                        reflector.pressure[band] * corner[band].powi(extra as i32)
                    }),
                    extra,
                    field.polyline(entry, &[bounce, listener]),
                    Some(reflector.id),
                );
            }
        }

        let candidate_count = candidates.len();
        candidates.sort_by(|a, b| {
            b.predicted_pressure
                .total_cmp(&a.predicted_pressure)
                .then(
                    a.geometry
                        .physical_path_length_m
                        .total_cmp(&b.geometry.physical_path_length_m),
                )
                .then(a.geometry.stable_path_id.cmp(&b.geometry.stable_path_id))
        });
        let mut taps: Vec<PlannedPath> = Vec::with_capacity(MAX_TAPS);
        for candidate in candidates {
            if taps.len() == MAX_TAPS {
                break;
            }
            if taps
                .iter()
                .all(|tap| (tap.excess_s - candidate.excess_s).abs() >= MIN_TAP_SEPARATION_S)
            {
                taps.push(candidate);
            }
        }
        EchoPathPlan {
            straight_line_m,
            primary_route_m: Some(primary_m),
            timed_route,
            line_of_sight,
            primary_arrival: field.position(primary_last).enu(),
            primary_polyline_enu_m: field.polyline(primary_last, &[listener]),
            primary_corners: primary_chain.len(),
            primary_turns,
            candidates: candidate_count,
            taps,
        }
    }
}

/// Audio-thread half of host echoes: a source's shot count is its trigger
/// generation, stored in the callback that feeds the shot's first sample.
pub(crate) struct ShotTrigger {
    trigger: EchoTrigger,
    sources: [bool; MAX_ACTIVE_SOURCES],
}

impl ShotTrigger {
    #[cfg_attr(not(feature = "live-output"), allow(dead_code))]
    pub(crate) fn shot_started(&self, source_index: usize, shots: u64) {
        if self.sources.get(source_index) == Some(&true) {
            self.trigger.trigger(source_index, shots);
        }
    }
}

/// Control-thread half: plans one-shot impulsive sources and publishes the
/// plan before their shot is retriggered. Looping sources keep the backend's
/// descriptor onsets and analytic plan.
pub(crate) struct HostEchoes {
    planner: EchoPathPlanner,
    publisher: EchoPlanPublisher,
    fields: Vec<Option<RouteField>>,
    published_plans: Vec<Option<EchoPathPlan>>,
    /// Last published pathing route per source, and where it was solved.
    routes: [Option<PrimaryRoute>; MAX_ACTIVE_SOURCES],
    routed_listener: Option<V3>,
    trigger: EchoTrigger,
    reported_transfer_fallbacks: u32,
}

impl HostEchoes {
    /// `triggered[i]` is the static position of each one-shot echo source.
    pub(crate) fn start(
        mut control: EchoTriggerControl,
        mesh: &AcousticMesh,
        materials: &MaterialTable,
        triggered: &[Option<EnuVector3>],
        listener: EnuVector3,
        air_exponents: [f32; 3],
    ) -> Result<(Self, ShotTrigger), String> {
        let mut planner = EchoPathPlanner::new(mesh, materials)?;
        planner.set_air_exponents(air_exponents);
        control.plans.set_air_exponents(air_exponents)
            .map_err(|error| format!("invalid scene air: {error:?}"))?;
        let fields = triggered
            .iter()
            .map(|source| source.map(|position| planner.route_field(position)))
            .collect();
        let mut sources = [false; MAX_ACTIVE_SOURCES];
        for (flag, source) in sources.iter_mut().zip(triggered) {
            *flag = source.is_some();
        }
        let mut echoes = Self {
            planner,
            publisher: control.plans,
            fields,
            published_plans: vec![None; triggered.len()],
            routes: [None; MAX_ACTIVE_SOURCES],
            routed_listener: None,
            trigger: control.trigger.clone(),
            reported_transfer_fallbacks: 0,
        };
        for (index, source) in triggered.iter().enumerate() {
            if let Some(source) = source {
                echoes.publish(index, *source, listener)?;
            }
        }
        Ok((
            echoes,
            ShotTrigger {
                trigger: control.trigger,
                sources,
            },
        ))
    }

    /// New shots use the selected air; shots already playing keep their plan.
    pub(crate) fn set_air_exponents(&mut self, air_exponents: [f32; 3]) {
        self.planner.set_air_exponents(air_exponents);
        self.publisher.set_air_exponents(air_exponents).expect("validated scene air");
    }

    /// Replans one triggered source for `listener`, rebuilding its street
    /// routes only if the source has moved. `None` for untriggered sources.
    pub(crate) fn publish(
        &mut self,
        source_index: usize,
        source: EnuVector3,
        listener: EnuVector3,
    ) -> Result<Option<EchoPathPlan>, String> {
        let Some(Some(field)) = self.fields.get_mut(source_index) else {
            return Ok(None);
        };
        if V3::from_enu(source).sub(field.source).length() > ROUTE_FIELD_REBUILD_M {
            *field = self.planner.route_field(source);
        }
        let plan = self.planner.plan(field, listener);
        self.publish_route(source_index, plan.timed_route)?;
        let planned = self
            .publisher
            .publish(source_index, plan.primary(), &plan.geometry())
            .map_err(|error| format!("echo plan for source {source_index}: {error:?}"))?;
        self.published_plans[source_index] = Some(plan.clone());
        eprintln!("[echo] source {source_index}: {}", plan.summary());
        for tap in planned.as_slice() {
            if let Some(relative) = tap.primary_relative_gain {
                let [low, mid, high] = relative.map(|gain| 20.0 * gain.max(1.0e-12).log10());
                eprintln!(
                    "  id={:#010x} over the rendered primary: {low:.1} / {mid:.1} / {high:.1} dB in the 0-0.8 / 0.8-8 / 8-22 kHz bands",
                    tap.stable_path_id
                );
            }
        }
        Ok(Some(plan))
    }

    pub(crate) fn published_plan(&self, source_index: usize) -> Option<&EchoPathPlan> {
        self.published_plans.get(source_index)?.as_ref()
    }

    /// Rebuilds a moved triggered source's street routes and retimes its baked
    /// pathing for `listener` at once, so neither the route nor
    /// [`Self::follow_listener`] keeps the old source position. Frozen shot
    /// echoes keep their plan until the next shot.
    pub(crate) fn move_source(
        &mut self,
        source_index: usize,
        source: EnuVector3,
        listener: EnuVector3,
    ) {
        let Some(Some(field)) = self.fields.get_mut(source_index) else {
            return;
        };
        if V3::from_enu(source).sub(field.source).length() <= ROUTE_FIELD_REBUILD_M {
            return;
        }
        *field = self.planner.route_field(source);
        let route = self.planner.timed_route(field, listener);
        if let Err(error) = self.publish_route(source_index, route) {
            eprintln!("[echo] {error}");
        }
    }

    /// Retimes each triggered source's baked pathing along its street route
    /// for `listener`; frozen shot echoes keep their plan.
    pub(crate) fn follow_listener(&mut self, listener: EnuVector3) {
        let position = V3::from_enu(listener);
        if self
            .routed_listener
            .is_some_and(|routed| position.sub(routed).length() < ROUTE_FOLLOW_M)
        {
            return;
        }
        self.routed_listener = Some(position);
        for source_index in 0..self.fields.len() {
            let Some(field) = &self.fields[source_index] else {
                continue;
            };
            let route = self.planner.timed_route(field, listener);
            if let Err(error) = self.publish_route(source_index, route) {
                eprintln!("[echo] {error}");
            }
        }
    }

    fn publish_route(
        &mut self,
        source_index: usize,
        route: Option<PrimaryRoute>,
    ) -> Result<(), String> {
        if self.routes[source_index] != route {
            self.publisher
                .publish_route(source_index, route)
                .map_err(|error| format!("pathing route for source {source_index}: {error:?}"))?;
            self.routes[source_index] = route;
        }
        Ok(())
    }

    /// Logs, once per source and off the audio thread, a shot whose routed
    /// primary had no baked-path transfer, so its echoes kept free-field levels.
    pub(crate) fn report_transfer_fallbacks(&mut self) {
        let fresh = self.trigger.take_transfer_fallbacks() & !self.reported_transfer_fallbacks;
        self.reported_transfer_fallbacks |= fresh;
        for index in (0..MAX_ACTIVE_SOURCES).filter(|index| fresh & (1 << index) != 0) {
            eprintln!(
                "[echo] source {index}: no baked-path primary at the shot; its echoes kept free-field levels"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use fightbox_world::Material;

    use super::*;

    fn materials() -> MaterialTable {
        MaterialTable::new(BTreeMap::from([(
            "brick".to_owned(),
            Material {
                absorption: [0.03, 0.04, 0.07],
                scattering: 0.15,
                transmission: [0.0; 3],
            },
        )]))
    }

    /// A vertical quad from `a` to `b`; its front face is to the right when
    /// walking from `a` to `b` (seen from above).
    fn wall(mesh: &mut AcousticMesh, a: [f32; 2], b: [f32; 2], height: f32) {
        let base = mesh.vertices_enu_m.len() as u32;
        mesh.vertices_enu_m.extend([
            EnuVector3::new(a[0], a[1], 0.0),
            EnuVector3::new(b[0], b[1], 0.0),
            EnuVector3::new(b[0], b[1], height),
            EnuVector3::new(a[0], a[1], height),
        ]);
        mesh.triangles
            .extend([[base, base + 1, base + 2], [base, base + 2, base + 3]]);
        mesh.material_ids.extend([0, 0]);
    }

    /// A closed box with outward faces and a roof.
    fn building(mesh: &mut AcousticMesh, min: [f32; 2], max: [f32; 2], height: f32) {
        let corners = [
            [min[0], min[1]],
            [max[0], min[1]],
            [max[0], max[1]],
            [min[0], max[1]],
        ];
        for index in 0..4 {
            wall(mesh, corners[index], corners[(index + 1) % 4], height);
        }
        let base = mesh.vertices_enu_m.len() as u32;
        mesh.vertices_enu_m.extend(
            corners
                .iter()
                .map(|corner| EnuVector3::new(corner[0], corner[1], height)),
        );
        mesh.triangles
            .extend([[base, base + 1, base + 2], [base, base + 2, base + 3]]);
        mesh.material_ids.extend([0, 0]);
    }

    fn empty() -> AcousticMesh {
        AcousticMesh {
            vertices_enu_m: Vec::new(),
            triangles: Vec::new(),
            material_ids: Vec::new(),
        }
    }

    fn plan(mesh: &AcousticMesh, source: [f32; 2], listener: [f32; 2]) -> EchoPathPlan {
        let planner = EchoPathPlanner::new(mesh, &materials()).unwrap();
        let field = planner.route_field(EnuVector3::new(source[0], source[1], 1.5));
        let plan = planner.plan(&field, EnuVector3::new(listener[0], listener[1], 1.5));
        eprintln!("{}", plan.summary());
        plan
    }

    #[test]
    fn a_single_facade_gives_exactly_one_source_reflection() {
        let mut mesh = empty();
        // West-facing facade at x = 30.
        wall(&mut mesh, [30.0, 20.0], [30.0, -20.0], 12.0);
        let plan = plan(&mesh, [0.0, 0.0], [0.0, 5.0]);
        assert!(plan.line_of_sight);
        assert_eq!(plan.timed_route, None);
        assert_eq!(plan.taps.len(), 1);
        let tap = &plan.taps[0];
        let expected = (60.0_f64 * 60.0 + 5.0 * 5.0).sqrt();
        assert_eq!(
            tap.route,
            EchoRoute::RoutedReflection {
                entry_is_source: true
            }
        );
        assert_eq!(tap.geometry.kind, EchoPathKind::Specular);
        assert!((f64::from(tap.geometry.physical_path_length_m) - expected).abs() < 1.0e-3);
        // Line of sight: the primary is the straight line, so the rendered
        // delay path equals the physical path.
        assert!(
            (tap.geometry.render_delay_path_m - tap.geometry.physical_path_length_m).abs() < 1.0e-3
        );
        let arrival = tap.geometry.arrival_position_enu;
        assert!((arrival.east_m - 30.0).abs() < 1.0e-4 && (arrival.north_m - 2.5).abs() < 1.0e-4);
        let brick_1k = (1.0_f64 - 0.04).sqrt() * (1.0_f64 - 0.15).sqrt();
        assert!((f64::from(tap.geometry.band_pressure_gain[1]) - brick_1k).abs() < 1.0e-5);
        assert!((tap.excess_s - (expected - 5.0) / SPEED_OF_SOUND_MPS).abs() < 1.0e-6);
    }

    #[test]
    fn scene_air_changes_echo_ranking_without_retiming() {
        let mut mesh = empty();
        wall(&mut mesh, [30.0, 20.0], [30.0, -20.0], 12.0);
        let mut planner = EchoPathPlanner::new(&mesh, &materials()).unwrap();
        assert_eq!(
            planner.air_exponents.map(f32::to_bits),
            FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M.map(f32::to_bits)
        );
        let field = planner.route_field(EnuVector3::new(0.0, 0.0, 1.5));
        let listener = EnuVector3::new(0.0, 5.0, 1.5);
        let fallback = planner.plan(&field, listener);
        let scene_air = [0.001, 0.004, 0.009];
        planner.set_air_exponents(scene_air);
        let selected = planner.plan(&field, listener);
        assert_eq!(selected.taps.len(), 1);
        let tap = &selected.taps[0];
        assert_eq!(tap.geometry, fallback.taps[0].geometry);
        assert_eq!(tap.excess_s, fallback.taps[0].excess_s);
        let length = (60.0_f64 * 60.0 + 5.0 * 5.0).sqrt();
        let expected = [0.03_f32, 0.04, 0.07]
            .into_iter()
            .zip(scene_air)
            .map(|(absorption, exponent)| {
                (1.0_f64 - f64::from(absorption)).sqrt()
                    * (1.0_f64 - f64::from(0.15_f32)).sqrt()
                    * (-f64::from(exponent) * (length - 5.0)).exp()
            })
            .sum::<f64>()
            * 5.0
            / (3.0 * length);
        assert!((tap.predicted_pressure - expected).abs() < 1.0e-12);
        assert_ne!(tap.predicted_pressure, fallback.taps[0].predicted_pressure);
    }

    #[test]
    fn a_back_facing_facade_is_rejected() {
        let mut mesh = empty();
        // Same plane, wound to face east, away from both endpoints.
        wall(&mut mesh, [30.0, -20.0], [30.0, 20.0], 12.0);
        assert!(plan(&mesh, [0.0, 0.0], [0.0, 5.0]).taps.is_empty());
    }

    #[test]
    fn a_bounce_off_the_finite_patch_is_rejected() {
        let mut mesh = empty();
        // The infinite plane's bounce point (y = 2.5) misses this patch.
        wall(&mut mesh, [30.0, 40.0], [30.0, 20.0], 12.0);
        assert!(plan(&mesh, [0.0, 0.0], [0.0, 5.0]).taps.is_empty());
    }

    #[test]
    fn an_occluded_leg_is_rejected() {
        let mut mesh = empty();
        wall(&mut mesh, [30.0, 20.0], [30.0, -20.0], 12.0);
        // A free-standing screen crosses source -> bounce (y = 1.25 at x = 15)
        // but neither bounce -> listener (y = 3.75) nor the direct path.
        wall(&mut mesh, [15.0, -5.0], [15.0, 2.0], 12.0);
        let plan = plan(&mesh, [0.0, 0.0], [0.0, 5.0]);
        assert!(plan.line_of_sight);
        assert!(plan.taps.is_empty());
    }

    #[test]
    fn a_street_route_around_one_building_is_the_primary_and_never_a_tap() {
        let mut mesh = empty();
        building(&mut mesh, [10.0, -10.0], [30.0, 10.0], 12.0);
        let plan = plan(&mesh, [0.0, 0.0], [40.0, 0.0]);
        assert!(!plan.line_of_sight);
        assert_eq!(plan.primary_corners, 2);
        let expected = 2.0 * 200.0_f64.sqrt() + 20.0;
        let primary = plan.primary_route_m.unwrap();
        assert!((primary - expected).abs() < 0.5, "{primary} vs {expected}");
        // The equal route round the other side is not a later arrival.
        assert!(plan.taps.is_empty());

        // Pathing is timed on the primary; the two sides are distinct routes
        // of equal length.
        let planner = EchoPathPlanner::new(&mesh, &materials()).unwrap();
        let field = planner.route_field(EnuVector3::new(0.0, 0.0, 1.5));
        let timed = |north: f32| planner.timed_route(&field, EnuVector3::new(40.0, north, 1.5));
        assert_eq!(timed(0.0), plan.timed_route);
        assert!((f64::from(timed(0.0).unwrap().length_m) - primary).abs() < 1.0e-3);
        let (north, south) = (timed(1.0).unwrap(), timed(-1.0).unwrap());
        assert_ne!(north.topology_id, south.topology_id);
        assert!((north.length_m - south.length_m).abs() < 1.0e-3);
        assert_eq!(
            planner.timed_route(&field, EnuVector3::new(0.0, 5.0, 1.5)),
            None
        );
    }

    #[test]
    fn a_routed_reflection_inherits_the_route_and_re_references_its_delay() {
        let mut mesh = empty();
        building(&mut mesh, [10.0, -10.0], [30.0, 10.0], 12.0);
        // West-facing facade behind the listener.
        wall(&mut mesh, [60.0, 30.0], [60.0, -30.0], 12.0);
        let plan = plan(&mesh, [0.0, 0.0], [40.0, 0.0]);
        let primary = plan.primary_route_m.unwrap();
        assert_eq!(plan.taps.len(), 1, "{}", plan.summary());
        let tap = &plan.taps[0];
        assert_eq!(
            tap.route,
            EchoRoute::RoutedReflection {
                entry_is_source: false
            }
        );
        assert_eq!(tap.charged_corners, 0);
        // Route to the last corner, then corner -> facade -> listener.
        let corner = V3::new(30.0 + 0.0707, 10.0 + 0.0707, 1.5);
        let image = V3::new(120.0 - corner.x, corner.y, 1.5);
        let expected = (primary - V3::new(40.0, 0.0, 1.5).sub(corner).length())
            + V3::new(40.0, 0.0, 1.5).sub(image).length();
        let physical = f64::from(tap.geometry.physical_path_length_m);
        assert!(
            (physical - expected).abs() < 0.05,
            "{physical} vs {expected}"
        );
        // Pathing plays the primary on its route, so the echo renders at its
        // own length.
        let route = plan.timed_route.expect("a routed primary times pathing");
        assert!((f64::from(route.length_m) - primary).abs() < 1.0e-3);
        let render = f64::from(tap.geometry.render_delay_path_m);
        assert!((render - physical).abs() < 1.0e-3);
        assert!(tap.excess_s >= MIN_EXCESS_S && tap.excess_s <= MAX_EXCESS_S);
        assert!((tap.geometry.arrival_position_enu.east_m - 60.0).abs() < 1.0e-3);
    }

    /// The retained megablock package at the street demo's two spots. Prints
    /// the plan; asserts only geometric sanity.
    #[test]
    fn megablock_street_spots_report_their_planned_echoes() {
        // Canonical checkout layout; an isolated worktree names it explicitly.
        let package = std::env::var_os("FIGHTBOX_MEGABLOCK_PACKAGE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../evidence/megablock-seed1/megablock.fightbox")
            });
        if !package.exists() {
            eprintln!(
                "SKIP: retained megablock package {} is absent; set FIGHTBOX_MEGABLOCK_PACKAGE",
                package.display()
            );
            return;
        }
        let loaded = fightbox_world::read_package(&package).unwrap();
        let started = std::time::Instant::now();
        let planner = EchoPathPlanner::new(&loaded.mesh, &loaded.materials).unwrap();
        let field = planner.route_field(EnuVector3::new(102.5, 102.5, 1.5));
        eprintln!(
            "megablock: {} reflectors, {} corners, route field {:?}",
            planner.reflector_count(),
            planner.corner_count(),
            started.elapsed()
        );
        for (label, east) in [("Spot A", 434.02_f32), ("Spot B", 438.02_f32)] {
            let started = std::time::Instant::now();
            let plan = planner.plan(&field, EnuVector3::new(east, 483.82, 1.5));
            eprintln!("{label} ({:?}): {}", started.elapsed(), plan.summary());
            let started = std::time::Instant::now();
            let route = planner.timed_route(&field, EnuVector3::new(east, 483.82, 1.5));
            eprintln!("{label} timed route ({:?}): {route:?}", started.elapsed());
            assert_eq!(route, plan.timed_route);
            assert!(!plan.line_of_sight);
            let primary = plan
                .primary_route_m
                .expect("a street route reaches the spot");
            assert!(primary >= plan.straight_line_m);
            assert!(plan.taps.len() <= MAX_TAPS);
            let listener = V3::new(f64::from(east), 483.82, 1.5);
            for tap in &plan.taps {
                assert!((MIN_EXCESS_S..=MAX_EXCESS_S).contains(&tap.excess_s));
                assert!(tap.geometry.physical_path_length_m as f64 > primary);
                let arrival = V3::from_enu(tap.geometry.arrival_position_enu);
                if let EchoRoute::RoutedReflection { .. } = tap.route {
                    let reflector = planner
                        .reflectors
                        .iter()
                        .find(|reflector| {
                            reflector.triangles.iter().any(|triangle| {
                                point_in_triangle(
                                    arrival,
                                    planner.occluder.triangles[*triangle as usize],
                                )
                            })
                        })
                        .expect("bounce lies on a finite patch");
                    assert!(reflector.normal.dot(listener) - reflector.offset > 0.0);
                    assert!(
                        planner
                            .occluder
                            .clear(arrival, listener, &reflector.triangles)
                    );
                } else {
                    assert!(planner.occluder.clear(arrival, listener, &[]));
                }
            }
        }
    }
}
