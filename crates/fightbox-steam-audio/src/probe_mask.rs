//! Floor-probe filter applied between uniform-floor generation and the path
//! bake.
//!
//! `IPL_PROBEGENERATIONTYPE_UNIFORMFLOOR` fills every column of the probe
//! volume, including the inside of every building taller than the probe
//! ceiling and the roof of every lower one. Those probes see nothing a street
//! listener can reach, yet each one is a row and a column of the all-pairs path
//! data, so the bake grows with their square. A city also needs only the
//! streets its listener walks. [`ProbeMask`] drops both kinds of probe before
//! `iplProbeBatchAddProbe` builds the batch; an empty mask keeps the legacy
//! `iplProbeBatchAddProbeArray` route and its byte-identical output.
//!
//! [`GradedDensity`] thins the same lattice instead of cutting it: full density
//! where the listener walks, a nested sparse subset everywhere else, repaired
//! so the sparse layer covers every street the full lattice covered and stays
//! one connected graph wherever the full lattice was.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::elevated_probes::{axis_samples, segment_hit, triangle_vertices, upward_ray_hit};
use crate::{EnuVector3, ProbeVolume, SceneMesh};

/// Horizontal cell edge of the triangle index, in metres.
const INDEX_CELL_M: f32 = 8.0;
/// Height above the probe-volume floor from which ground-standing solids are
/// detected. Low enough that a garage, fence or embankment is always crossed,
/// high enough to sit clear of the ground quad and building floors.
const GROUND_PROBE_Z_M: f32 = 0.05;
/// Sparsest graded layer: eight lattice steps (32 m at 4 m spacing).
const MAX_COARSE_STRIDE: u32 = 8;

/// Which generated floor probes the bake keeps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeMask {
    /// Drop probes whose column stands on a closed solid rising from the
    /// volume floor: building interiors, and probes the generator placed on a
    /// roof lower than the probe ceiling. A floating closed slab (an elevated
    /// rail deck) is crossed twice and keeps the street probes beneath it.
    pub drop_over_solids: bool,
    /// Keep only probes near a walked route. `None` keeps the whole volume.
    pub corridor: Option<ProbeCorridor>,
    /// Thin the lattice away from the walked areas. `None` keeps full density.
    pub graded: Option<GradedDensity>,
}

/// Graded density on the one generated lattice: every point where the listener
/// walks, every `coarse_stride`-th point per axis elsewhere. The sparse layer is
/// therefore a subset of the fine one, phase-locked to it, not a second grid.
///
/// A sparse probe's influence radius is `coarse_stride` times the generated
/// one, so the sparse layer leaves no point outside every probe sphere.
#[derive(Clone, Debug, PartialEq)]
pub struct GradedDensity {
    /// Lattice stride of the sparse layer: 2 keeps every other 4 m point (8 m).
    pub coarse_stride: u32,
    /// Where every lattice point is kept: walked routes, plus disks around the
    /// scene centre, street corners and platforms.
    pub fine: ProbeCorridor,
}

/// A walked route, buffered to a strip, plus disks around sources off it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeCorridor {
    /// Route polylines as ENU east/north metres.
    pub routes_enu_m: Vec<Vec<[f32; 2]>>,
    /// Probes within this horizontal distance of any route segment are kept.
    pub half_width_m: f32,
    /// Extra kept disks as `(east/north centre, radius)`, so a source standing
    /// off the route still has an influencing probe for pathing.
    pub islands_enu_m: Vec<([f32; 2], f32)>,
}

impl ProbeMask {
    /// Whether this mask keeps every generated probe.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.drop_over_solids && self.corridor.is_none() && self.graded.is_none()
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if let Some(corridor) = &self.corridor {
            corridor.validate()?;
        }
        if let Some(graded) = &self.graded {
            if !(2..=MAX_COARSE_STRIDE).contains(&graded.coarse_stride) {
                return Err("graded probe stride must be between 2 and 8 lattice steps");
            }
            graded.fine.validate()?;
        }
        Ok(())
    }

    /// Prepares the mesh index once for many probe queries.
    pub(crate) fn compile<'a>(
        &'a self,
        mesh: &'a SceneMesh,
        volume: ProbeVolume,
    ) -> CompiledMask<'a> {
        let mut cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        let mut sight_cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        if self.drop_over_solids || self.graded.is_some() {
            for (index, triangle) in mesh.triangles.iter().enumerate() {
                let Some(vertices) = triangle_vertices(mesh, *triangle) else {
                    continue;
                };
                let [min_x, max_x] = span(vertices.map(|vertex| vertex.x));
                let [min_y, max_y] = span(vertices.map(|vertex| vertex.y));
                // A vertical wall can never be crossed by a vertical ray, but
                // it is exactly what blocks the sight line between two probes.
                let area = (vertices[1].x - vertices[0].x) * (vertices[2].y - vertices[0].y)
                    - (vertices[2].x - vertices[0].x) * (vertices[1].y - vertices[0].y);
                let upward = self.drop_over_solids && area.abs() > 1.0e-9;
                for cell_x in cell(min_x)..=cell(max_x) {
                    for cell_y in cell(min_y)..=cell(max_y) {
                        if upward {
                            cells.entry((cell_x, cell_y)).or_default().push(index);
                        }
                        if self.graded.is_some() {
                            sight_cells.entry((cell_x, cell_y)).or_default().push(index);
                        }
                    }
                }
            }
        }
        let east = axis_samples(volume.min_enu_m.x, volume.max_enu_m.x, volume.spacing_m);
        let north = axis_samples(volume.min_enu_m.y, volume.max_enu_m.y, volume.spacing_m);
        CompiledMask {
            mask: self,
            mesh,
            cells,
            sight_cells,
            ground_z_m: volume.min_enu_m.z + GROUND_PROBE_Z_M,
            lattice_origin: [
                east.first().copied().unwrap_or(volume.min_enu_m.x),
                north.first().copied().unwrap_or(volume.min_enu_m.y),
            ],
            spacing_m: volume.spacing_m,
        }
    }
}

impl ProbeCorridor {
    fn validate(&self) -> Result<(), &'static str> {
        if !self.half_width_m.is_finite() || self.half_width_m <= 0.0 {
            return Err("probe corridor half width must be finite and positive");
        }
        if self.routes_enu_m.is_empty() && self.islands_enu_m.is_empty() {
            return Err("probe corridor needs a route or an island");
        }
        let finite = |point: &[f32; 2]| point.iter().all(|value| value.is_finite());
        if !self.routes_enu_m.iter().flatten().all(finite)
            || !self
                .islands_enu_m
                .iter()
                .all(|(centre, radius)| finite(centre) && radius.is_finite() && *radius > 0.0)
        {
            return Err("probe corridor coordinates and island radii must be finite");
        }
        Ok(())
    }
}

pub(crate) struct CompiledMask<'a> {
    mask: &'a ProbeMask,
    mesh: &'a SceneMesh,
    cells: HashMap<(i32, i32), Vec<usize>>,
    /// Every triangle, walls included, for probe-to-probe sight lines.
    sight_cells: HashMap<(i32, i32), Vec<usize>>,
    ground_z_m: f32,
    /// First uniform-floor sample per axis, the lattice phase.
    lattice_origin: [f32; 2],
    spacing_m: f32,
}

impl CompiledMask<'_> {
    /// Kept probes, in input order, with the factor that scales each one's
    /// generated influence radius. Only a graded mask scales: its sparse
    /// probes reach `coarse_stride` times as far.
    pub(crate) fn select(&self, centres: &[EnuVector3]) -> Vec<Option<f32>> {
        match &self.mask.graded {
            Some(graded) => self.graded_selection(graded, centres),
            None => centres
                .iter()
                .map(|centre| self.keeps(*centre).then_some(1.0))
                .collect(),
        }
    }

    pub(crate) fn keeps(&self, centre: EnuVector3) -> bool {
        if let Some(corridor) = &self.mask.corridor {
            if !corridor.contains([centre.x, centre.y]) {
                return false;
            }
        }
        !(self.mask.drop_over_solids && self.stands_on_solid(centre))
    }

    /// Ray parity from just above the floor straight up, as in
    /// `elevated_probes::is_inside_solid`, restricted to the probe's cell.
    fn stands_on_solid(&self, centre: EnuVector3) -> bool {
        let origin = EnuVector3::new(centre.x, centre.y, self.ground_z_m);
        let Some(candidates) = self.cells.get(&(cell(centre.x), cell(centre.y))) else {
            return false;
        };
        let mut crossings: Vec<f64> = candidates
            .iter()
            .filter_map(|&index| {
                let vertices = triangle_vertices(self.mesh, self.mesh.triangles[index])?;
                upward_ray_hit(origin, vertices)
            })
            .collect();
        // A ray grazing a shared roof diagonal reports each adjacent triangle.
        crossings.sort_by(f64::total_cmp);
        crossings.dedup_by(|left, right| (*left - *right).abs() <= 1.0e-6);
        crossings.len() % 2 == 1
    }

    /// The graded subset of the probes [`Self::keeps`] accepts.
    ///
    /// Probes become nodes of a sight graph whose edges join lattice
    /// neighbours up to one sparse step apart that see each other. Every fine
    /// and every phase-aligned sparse probe is kept; then, in lattice order so
    /// generator order cannot change the result:
    /// 1. a probe no kept neighbour's sphere covers is kept, which threads
    ///    sparse probes along fenced alleys and gangways the sparse phase
    ///    misses;
    /// 2. kept probes split into several groups inside one connected part of
    ///    the full graph are joined along the shortest full-graph path, so the
    ///    sparse layer has no island the full lattice did not have.
    fn graded_selection(&self, graded: &GradedDensity, centres: &[EnuVector3]) -> Vec<Option<f32>> {
        let stride = graded.coarse_stride as i32;
        let mut sites: Vec<((i32, i32), usize)> = centres
            .iter()
            .enumerate()
            .filter(|(_, centre)| self.keeps(**centre))
            .map(|(index, centre)| (self.site(*centre), index))
            .collect();
        sites.sort_unstable();
        // The uniform-floor generator places one probe per column.
        sites.dedup_by_key(|(site, _)| *site);
        let count = sites.len();
        let lookup: HashMap<(i32, i32), usize> = sites
            .iter()
            .enumerate()
            .map(|(node, (site, _))| (*site, node))
            .collect();
        let point = |node: usize| centres[sites[node].1];
        let fine: Vec<bool> = (0..count)
            .map(|node| graded.fine.contains([point(node).x, point(node).y]))
            .collect();

        let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); count];
        for node in 0..count {
            let (east, north) = sites[node].0;
            for step_east in 0..=stride {
                for step_north in -stride..=stride {
                    // Each unordered pair once.
                    if step_east == 0 && step_north <= 0 {
                        continue;
                    }
                    let Some(&other) = lookup.get(&(east + step_east, north + step_north)) else {
                        continue;
                    };
                    if self.sees(point(node), point(other)) {
                        neighbours[node].push(other);
                        neighbours[other].push(node);
                    }
                }
            }
        }

        let scale = |node: usize| if fine[node] { 1.0 } else { graded.coarse_stride as f32 };
        let mut kept: Vec<bool> = (0..count)
            .map(|node| {
                let (east, north) = sites[node].0;
                fine[node] || (east.rem_euclid(stride) == 0 && north.rem_euclid(stride) == 0)
            })
            .collect();
        for node in 0..count {
            if kept[node] {
                continue;
            }
            let covered = neighbours[node].iter().any(|&other| {
                kept[other] && distance3(point(node), point(other)) < self.spacing_m * scale(other)
            });
            kept[node] = !covered;
        }

        let mut groups = UnionFind::new(count);
        let mut whole = UnionFind::new(count);
        for node in 0..count {
            for &other in &neighbours[node] {
                whole.union(node, other);
                if kept[node] && kept[other] {
                    groups.union(node, other);
                }
            }
        }
        let mut parts: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for node in (0..count).filter(|node| kept[*node]) {
            parts.entry(whole.find(node)).or_default().push(node);
        }
        let mut previous = vec![usize::MAX; count];
        let mut visited = vec![0_u32; count];
        let mut generation = 0_u32;
        for members in parts.values_mut() {
            loop {
                let anchor = groups.find(members[0]);
                if members.iter().all(|node| groups.find(*node) == anchor) {
                    break;
                }
                generation += 1;
                let mut queue: VecDeque<usize> = VecDeque::new();
                for &node in members.iter() {
                    if groups.find(node) == anchor {
                        visited[node] = generation;
                        queue.push_back(node);
                    }
                }
                let mut reached = None;
                'search: while let Some(node) = queue.pop_front() {
                    for &other in &neighbours[node] {
                        if visited[other] == generation {
                            continue;
                        }
                        visited[other] = generation;
                        previous[other] = node;
                        if kept[other] {
                            reached = Some(other);
                            break 'search;
                        }
                        queue.push_back(other);
                    }
                }
                // The whole graph joins every member, so the search always
                // reaches another group; nothing else can end it.
                let Some(reached) = reached else { break };
                let mut path = Vec::new();
                let mut node = previous[reached];
                while !kept[node] {
                    path.push(node);
                    node = previous[node];
                }
                for &node in &path {
                    kept[node] = true;
                }
                // Bridges seed later searches, which then stop only at
                // another group.
                members.extend(&path);
                for &node in path.iter().chain([&reached]) {
                    for &other in &neighbours[node] {
                        if kept[other] {
                            groups.union(node, other);
                        }
                    }
                }
                groups.union(anchor, reached);
            }
        }

        let mut selection = vec![None; centres.len()];
        for node in (0..count).filter(|node| kept[*node]) {
            selection[sites[node].1] = Some(scale(node));
        }
        selection
    }

    /// Lattice indices of a generated probe.
    fn site(&self, centre: EnuVector3) -> (i32, i32) {
        (
            ((centre.x - self.lattice_origin[0]) / self.spacing_m).round() as i32,
            ((centre.y - self.lattice_origin[1]) / self.spacing_m).round() as i32,
        )
    }

    /// Whether no triangle cuts the straight line between two probe centres.
    fn sees(&self, from: EnuVector3, to: EnuVector3) -> bool {
        let [min_x, max_x] = span([from.x, to.x, to.x]);
        let [min_y, max_y] = span([from.y, to.y, to.y]);
        for cell_x in cell(min_x)..=cell(max_x) {
            for cell_y in cell(min_y)..=cell(max_y) {
                let Some(candidates) = self.sight_cells.get(&(cell_x, cell_y)) else {
                    continue;
                };
                if candidates.iter().any(|&index| {
                    triangle_vertices(self.mesh, self.mesh.triangles[index])
                        .is_some_and(|vertices| segment_hit(from, to, vertices))
                }) {
                    return false;
                }
            }
        }
        true
    }
}

struct UnionFind(Vec<usize>);

impl UnionFind {
    fn new(count: usize) -> Self {
        Self((0..count).collect())
    }

    fn find(&mut self, node: usize) -> usize {
        let mut root = node;
        while self.0[root] != root {
            root = self.0[root];
        }
        let mut node = node;
        while self.0[node] != root {
            let next = self.0[node];
            self.0[node] = root;
            node = next;
        }
        root
    }

    fn union(&mut self, left: usize, right: usize) {
        let (left, right) = (self.find(left), self.find(right));
        if left != right {
            self.0[left.max(right)] = left.min(right);
        }
    }
}

impl ProbeCorridor {
    fn contains(&self, point: [f32; 2]) -> bool {
        self.islands_enu_m
            .iter()
            .any(|(centre, radius)| distance(point, *centre) <= *radius)
            || self.routes_enu_m.iter().any(|route| {
                route.len() == 1 && distance(point, route[0]) <= self.half_width_m
                    || route.windows(2).any(|segment| {
                        segment_distance(point, segment[0], segment[1]) <= self.half_width_m
                    })
            })
    }
}

/// Floor probes the mask would keep on the uniform-floor grid of `volume`,
/// for preflight estimates. The grid is the one `axis_samples` reproduces, at
/// the probe height above a flat floor at the volume's minimum altitude.
#[must_use]
pub fn masked_floor_probe_count(volume: ProbeVolume, mesh: &SceneMesh, mask: &ProbeMask) -> u64 {
    masked_floor_probes(volume, mesh, mask).len() as u64
}

/// The probes behind [`masked_floor_probe_count`], each with its
/// influence-radius scale (above 1 only for a graded mask's sparse probes).
#[must_use]
pub fn masked_floor_probes(
    volume: ProbeVolume,
    mesh: &SceneMesh,
    mask: &ProbeMask,
) -> Vec<(EnuVector3, f32)> {
    let east = axis_samples(volume.min_enu_m.x, volume.max_enu_m.x, volume.spacing_m);
    let north = axis_samples(volume.min_enu_m.y, volume.max_enu_m.y, volume.spacing_m);
    let height = volume.min_enu_m.z + volume.height_above_floor_m;
    let centres: Vec<EnuVector3> = east
        .iter()
        .flat_map(|x| north.iter().map(move |y| EnuVector3::new(*x, *y, height)))
        .collect();
    let selection = mask.compile(mesh, volume).select(&centres);
    centres
        .into_iter()
        .zip(selection)
        .filter_map(|(centre, scale)| scale.map(|scale| (centre, scale)))
        .collect()
}

fn span(values: [f32; 3]) -> [f32; 2] {
    [
        values[0].min(values[1]).min(values[2]),
        values[0].max(values[1]).max(values[2]),
    ]
}

fn cell(value: f32) -> i32 {
    (value / INDEX_CELL_M).floor() as i32
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn distance3(a: EnuVector3, b: EnuVector3) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

fn segment_distance(point: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let along = [b[0] - a[0], b[1] - a[1]];
    let length_squared = along[0] * along[0] + along[1] * along[1];
    if length_squared <= f32::EPSILON {
        return distance(point, a);
    }
    let t = (((point[0] - a[0]) * along[0] + (point[1] - a[1]) * along[1]) / length_squared)
        .clamp(0.0, 1.0);
    distance(point, [a[0] + t * along[0], a[1] + t * along[1]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AcousticMaterial;

    /// Closed boxes plus a ground quad, outward winding, as the world compiler emits.
    fn scene(boxes: &[([f32; 3], [f32; 3])]) -> SceneMesh {
        let mut vertices_enu_m = vec![
            EnuVector3::new(-50.0, -50.0, 0.0),
            EnuVector3::new(50.0, -50.0, 0.0),
            EnuVector3::new(50.0, 50.0, 0.0),
            EnuVector3::new(-50.0, 50.0, 0.0),
        ];
        let mut triangles = vec![[0, 1, 2], [0, 2, 3]];
        for (min, max) in boxes {
            let base = vertices_enu_m.len() as i32;
            for z in [min[2], max[2]] {
                vertices_enu_m.extend([
                    EnuVector3::new(min[0], min[1], z),
                    EnuVector3::new(max[0], min[1], z),
                    EnuVector3::new(max[0], max[1], z),
                    EnuVector3::new(min[0], max[1], z),
                ]);
            }
            for [a, b, c] in [
                [0, 2, 1],
                [0, 3, 2],
                [4, 5, 6],
                [4, 6, 7],
                [0, 1, 5],
                [0, 5, 4],
                [1, 2, 6],
                [1, 6, 5],
                [2, 3, 7],
                [2, 7, 6],
                [3, 0, 4],
                [3, 4, 7],
            ] {
                triangles.push([base + a, base + b, base + c]);
            }
        }
        SceneMesh {
            material_indices: vec![0; triangles.len()],
            triangles,
            vertices_enu_m,
            materials: vec![AcousticMaterial::MASONRY],
        }
    }

    fn volume(spacing_m: f32) -> ProbeVolume {
        ProbeVolume {
            min_enu_m: EnuVector3::new(-40.0, -40.0, 0.0),
            max_enu_m: EnuVector3::new(40.0, 40.0, 3.0),
            spacing_m,
            height_above_floor_m: 1.5,
        }
    }

    #[test]
    fn interiors_and_low_roofs_drop_while_streets_under_a_deck_stay() {
        let mesh = scene(&[
            ([-10.0, -10.0, 0.0], [10.0, 10.0, 12.0]), // building above the ceiling
            ([20.0, 20.0, 0.0], [26.0, 26.0, 2.8]),    // garage below the ceiling
            ([-30.0, 20.0, 5.0], [-20.0, 30.0, 5.5]),  // floating rail deck
        ]);
        let mask = ProbeMask {
            drop_over_solids: true,
            corridor: None,
            graded: None,
        };
        let compiled = mask.compile(&mesh, volume(4.0));
        assert!(
            !compiled.keeps(EnuVector3::new(0.0, 0.0, 1.5)),
            "interior probe kept"
        );
        assert!(
            !compiled.keeps(EnuVector3::new(23.0, 23.0, 4.3)),
            "garage-roof probe kept"
        );
        assert!(
            compiled.keeps(EnuVector3::new(-25.0, 25.0, 1.5)),
            "street under the deck dropped"
        );
        assert!(
            compiled.keeps(EnuVector3::new(30.0, -30.0, 1.5)),
            "open street dropped"
        );
        assert!(ProbeMask::default().is_empty());
    }

    #[test]
    fn corridor_keeps_the_route_strip_and_islands_only() {
        let mesh = scene(&[]);
        let mask = ProbeMask {
            drop_over_solids: false,
            corridor: Some(ProbeCorridor {
                routes_enu_m: vec![vec![[-40.0, 0.0], [40.0, 0.0]]],
                half_width_m: 6.0,
                islands_enu_m: vec![([30.0, 30.0], 5.0)],
            }),
            graded: None,
        };
        assert!(mask.validate().is_ok());
        let compiled = mask.compile(&mesh, volume(4.0));
        assert!(compiled.keeps(EnuVector3::new(10.0, 5.9, 1.5)));
        assert!(!compiled.keeps(EnuVector3::new(10.0, 6.1, 1.5)));
        assert!(compiled.keeps(EnuVector3::new(32.0, 32.0, 1.5)));
        // 21 columns at 4 m over 80 m; a 12 m strip covers three rows plus
        // the island's few columns.
        let total = masked_floor_probe_count(volume(4.0), &mesh, &ProbeMask::default());
        let kept = masked_floor_probe_count(volume(4.0), &mesh, &mask);
        assert_eq!(total, 21 * 21);
        assert!(kept > 3 * 21 && kept < 5 * 21, "kept {kept}");
        let empty = ProbeMask {
            drop_over_solids: false,
            corridor: Some(ProbeCorridor {
                routes_enu_m: Vec::new(),
                half_width_m: 6.0,
                islands_enu_m: Vec::new(),
            }),
            graded: None,
        };
        assert!(empty.validate().is_err());
    }

    fn graded(stride: u32, islands_enu_m: Vec<([f32; 2], f32)>) -> ProbeMask {
        ProbeMask {
            drop_over_solids: true,
            corridor: None,
            graded: Some(GradedDensity {
                coarse_stride: stride,
                fine: ProbeCorridor {
                    routes_enu_m: Vec::new(),
                    half_width_m: 6.0,
                    islands_enu_m,
                },
            }),
        }
    }

    /// Whether the probes form one graph, joining those within `reach_m` that
    /// see each other.
    fn connected(compiled: &CompiledMask<'_>, probes: &[(EnuVector3, f32)], reach_m: f32) -> bool {
        let mut seen = vec![false; probes.len()];
        let mut stack = vec![0];
        seen[0] = true;
        while let Some(node) = stack.pop() {
            for other in 0..probes.len() {
                if !seen[other]
                    && distance3(probes[node].0, probes[other].0) <= reach_m
                    && compiled.sees(probes[node].0, probes[other].0)
                {
                    seen[other] = true;
                    stack.push(other);
                }
            }
        }
        seen.iter().all(|reached| *reached)
    }

    #[test]
    fn graded_layer_is_the_nested_sparse_lattice_plus_every_walked_point() {
        let mesh = scene(&[]);
        let mask = graded(2, vec![([0.0, 0.0], 10.0)]);
        assert!(mask.validate().is_ok() && !mask.is_empty());
        let probes = masked_floor_probes(volume(4.0), &mesh, &mask);
        let full = masked_floor_probes(volume(4.0), &mesh, &ProbeMask::default());
        for (centre, scale) in &probes {
            assert!(full.iter().any(|(point, _)| point == centre), "{centre:?} off the lattice");
            let walked = centre.x.hypot(centre.y) <= 10.0;
            let sparse_phase = centre.x.rem_euclid(8.0) == 0.0 && centre.y.rem_euclid(8.0) == 0.0;
            assert!(walked || sparse_phase, "{centre:?} breaks the 8 m phase");
            assert_eq!(*scale, if walked { 1.0 } else { 2.0 });
        }
        // 11 x 11 sparse points over 80 m, plus the 16 off-phase points of the
        // 21 inside the 10 m disk. Open ground needs no repair.
        assert_eq!(full.len(), 21 * 21);
        assert_eq!(probes.len(), 11 * 11 + 16);
        assert!(graded(1, vec![([0.0, 0.0], 10.0)]).validate().is_err());
    }

    #[test]
    fn graded_layer_bridges_a_wall_gap_the_sparse_phase_misses() {
        // Thin walls between lattice columns 0 and 4 m and -16 and -12 m, open
        // only for 2.5..5.5 m north: the off-phase row at 4 m is the only way
        // through, so three sparse groups need two bridges.
        let mesh = scene(&[
            ([1.9, -40.0, 0.0], [2.1, 2.5, 3.0]),
            ([1.9, 5.5, 0.0], [2.1, 40.0, 3.0]),
            ([-14.1, -40.0, 0.0], [-13.9, 2.5, 3.0]),
            ([-14.1, 5.5, 0.0], [-13.9, 40.0, 3.0]),
        ]);
        let mask = graded(2, vec![([30.0, -30.0], 3.0)]);
        let compiled = mask.compile(&mesh, volume(4.0));
        let reach_m = 8.0 * std::f32::consts::SQRT_2 + 0.01;
        let probes = masked_floor_probes(volume(4.0), &mesh, &mask);
        let sparse: Vec<(EnuVector3, f32)> = probes
            .iter()
            .copied()
            .filter(|(centre, _)| centre.x.rem_euclid(8.0) == 0.0 && centre.y.rem_euclid(8.0) == 0.0)
            .collect();
        assert!(!connected(&compiled, &sparse, reach_m), "the wall should split the phase");
        assert!(connected(&compiled, &probes, reach_m), "graded layer left an island");
        assert!(
            probes.iter().any(|(centre, _)| centre.y == 4.0 && (centre.x == 0.0 || centre.x == 4.0)),
            "no probe threads the gap"
        );
        // A bridge probe per wall and the island, not a densified neighbourhood.
        assert!(probes.len() <= sparse.len() + 3 + 4, "{} vs {}", probes.len(), sparse.len());
    }
}
