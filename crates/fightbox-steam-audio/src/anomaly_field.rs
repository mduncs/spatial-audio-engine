//! Cheap acoustic-anomaly proxy metrics shared by offline sweeps and live trails.
//!
//! The query session is deliberately simulation-only: it constructs one retained
//! Steam Audio scene and baked path simulator, but no HRTF, render effect, mixer,
//! or reflection renderer. Callers must keep it on a control/background thread.

#[cfg(feature = "linked-sdk")]
use crate::AudioConfig;
use crate::{
    BackendError, BakedProbeBatch, EnuVector3, MultiSourceDescriptor, S3SimulationConfig, SceneMesh,
};

/// Small positive floor used only to keep diagnostic logarithms finite.
pub const ENERGY_FLOOR: f32 = 1.0e-30;
/// The proxy follows the engine's one-metre minimum source-distance convention.
pub const SOURCE_RADIUS_M: f32 = 1.0;
/// An unoccluded prediction below this is too quiet to call an inversion risk.
pub const MIN_MEANINGFUL_FREE_FIELD_DB: f32 = 35.0;
/// Eighteen decibels is the ratified minimum direct shadow for the proxy signature.
pub const MIN_INVERSION_DIRECT_LOSS_DB: f32 = 18.0;
/// A path send below -20 dB is weak enough not to plausibly fill a deep shadow.
pub const MAX_INVERSION_PATH_STRENGTH_DB: f32 = -20.0;
/// A cell must differ from every cardinal neighbour by this much to be a spike.
pub const SINGLE_CELL_OCCLUSION_SPIKE_DELTA: f32 = 0.70;
/// A 0.26 audibility change per metre exceeds the one-metre volumetric footprint.
pub const MAX_PLAUSIBLE_OCCLUSION_SLOPE_PER_M: f32 = 0.26;
/// A 24 dB isolated path jump is too large to accept without nearby corroboration.
pub const SINGLE_CELL_PATH_SPIKE_DB: f32 = 24.0;
/// Reflections more than 12 dB over direct+path are implausible enough to inspect.
pub const MAX_PLAUSIBLE_REFLECTION_EXCESS_DB: f32 = 12.0;
/// Live and adaptive persistence follows the study's five-fresh-poses-per-second cap.
pub const ADAPTIVE_LIVE_SAMPLE_HZ: u32 = 5;
/// Ignore tiny derivative chatter from simulator interpolation and float noise.
pub const OCCLUSION_DERIVATIVE_EPSILON_PER_M: f32 = 0.02;
/// Do not repeatedly fan out around essentially the same corner.
pub const INTERSECTION_TRIGGER_COOLDOWN_M: f32 = 4.0;

/// Stable anomaly classes persisted in field and trail artifacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum AnomalyClass {
    InversionSignature = 0,
    InvalidEnergy = 1,
    InvalidCoefficient = 2,
    NeighborSpike = 3,
    ExcessiveDiscontinuity = 4,
    ZeroPathWithCoverage = 5,
    ReflectionEnergyExcess = 6,
}

impl AnomalyClass {
    pub const ALL: [Self; 7] = [
        Self::InversionSignature,
        Self::InvalidEnergy,
        Self::InvalidCoefficient,
        Self::NeighborSpike,
        Self::ExcessiveDiscontinuity,
        Self::ZeroPathWithCoverage,
        Self::ReflectionEnergyExcess,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::InversionSignature => "inversion_signature",
            Self::InvalidEnergy => "invalid_energy",
            Self::InvalidCoefficient => "invalid_coefficient",
            Self::NeighborSpike => "neighbor_spike",
            Self::ExcessiveDiscontinuity => "excessive_discontinuity",
            Self::ZeroPathWithCoverage => "zero_path_with_coverage",
            Self::ReflectionEnergyExcess => "reflection_energy_excess",
        }
    }

    #[must_use]
    pub const fn rationale(self) -> &'static str {
        match self {
            Self::InversionSignature => {
                "meaningful free field plus deep direct shadow and weak baked fill"
            }
            Self::InvalidEnergy => {
                "NaN, infinity, or a denormal energy indicates computation trouble"
            }
            Self::InvalidCoefficient => {
                "direct audibility and path coefficients are normalized to [0,1]"
            }
            Self::NeighborSpike => {
                "one cell unlike every neighbour is unlikely to be a stable field"
            }
            Self::ExcessiveDiscontinuity => {
                "the spatial slope exceeds the simulator's volumetric resolution"
            }
            Self::ZeroPathWithCoverage => {
                "covered, outdoor source and listener endpoints should not yield an exact zero path"
            }
            Self::ReflectionEnergyExcess => {
                "reflection energy far above direct plus path exceeds the declared physical bound"
            }
        }
    }

    #[must_use]
    pub const fn threshold(self) -> &'static str {
        match self {
            Self::InversionSignature => "free>=35dB, direct_loss>=18dB, path<=-20dB",
            Self::InvalidEnergy => "non-finite or 0<abs(x)<f32::MIN_POSITIVE",
            Self::InvalidCoefficient => "outside [0,1]",
            Self::NeighborSpike => "occlusion delta>=0.70 or path delta>=24dB",
            Self::ExcessiveDiscontinuity => "occlusion slope>0.26/m",
            Self::ZeroPathWithCoverage => {
                "path_sh_energy==0 with both endpoints covered and outside static solids"
            }
            Self::ReflectionEnergyExcess => "reflection/(direct+path)>12dB",
        }
    }
}

/// Compact flags keep field rasters deterministic and cheap to draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnomalyFlags(pub u32);

impl AnomalyFlags {
    #[must_use]
    pub const fn contains(self, class: AnomalyClass) -> bool {
        self.0 & (1 << class as u8) != 0
    }

    pub fn insert(&mut self, class: AnomalyClass) {
        self.0 |= 1 << class as u8;
    }

    fn remove(&mut self, class: AnomalyClass) {
        self.0 &= !(1 << class as u8);
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Exact raw values returned by one direct/path query or one live snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnomalyRawSample {
    pub position_enu: EnuVector3,
    /// Steam Audio audibility convention: one is clear and zero is blocked.
    pub direct_audibility: f32,
    pub path_eq: [f32; 3],
    pub path_sh_energy: f32,
    pub path_coefficient_min: f32,
    pub path_coefficient_max: f32,
    pub source_probe_covered: bool,
    pub listener_probe_covered: bool,
    /// True when an upward ray from the source meets static scene geometry.
    /// Provider-generated city solids use this to reject emitter poses inside buildings.
    pub source_endpoint_inside_static_geometry: bool,
    /// True when an upward ray from the listener meets static scene geometry.
    /// Probe influence can extend through a wall, so coverage alone does not make
    /// such an endpoint a physically meaningful pathing query.
    pub listener_endpoint_inside_static_geometry: bool,
    /// Actual rendered stage energy when a live tap has a single audible source.
    pub direct_path_energy: Option<f64>,
    pub reflection_energy: Option<f64>,
}

/// Derived, display-ready proxy cell. Neighbour classes are added in a second pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProxyCell {
    pub position_enu: EnuVector3,
    pub direct_audibility: f32,
    pub direct_loss_db: f32,
    pub path_sh_energy: f32,
    pub path_strength_db: f32,
    pub free_field_db: f32,
    pub score: f32,
    pub source_probe_covered: bool,
    pub listener_probe_covered: bool,
    pub direct_path_energy: Option<f64>,
    pub reflection_energy: Option<f64>,
    pub reflection_excess_db: Option<f32>,
    pub flags: AnomalyFlags,
}

/// Cell-centred rectangular grid, including correctly centred partial edge cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSpec {
    pub min_enu: [f32; 2],
    pub max_enu: [f32; 2],
    pub listener_height_m: f32,
    pub spacing_m: f32,
}

impl GridSpec {
    pub fn validate(self) -> Result<Self, &'static str> {
        if !self.min_enu.into_iter().all(f32::is_finite)
            || !self.max_enu.into_iter().all(f32::is_finite)
            || !self.listener_height_m.is_finite()
            || !self.spacing_m.is_finite()
            || self.spacing_m <= 0.0
            || self.max_enu[0] <= self.min_enu[0]
            || self.max_enu[1] <= self.min_enu[1]
        {
            return Err(
                "grid requires finite increasing bounds, finite height, and positive spacing",
            );
        }
        Ok(self)
    }

    #[must_use]
    pub fn width(self) -> usize {
        axis_count(self.min_enu[0], self.max_enu[0], self.spacing_m)
    }

    #[must_use]
    pub fn height(self) -> usize {
        axis_count(self.min_enu[1], self.max_enu[1], self.spacing_m)
    }

    #[must_use]
    pub fn cell_count(self) -> usize {
        self.width().saturating_mul(self.height())
    }

    #[must_use]
    pub fn position(self, index: usize) -> EnuVector3 {
        let width = self.width();
        let column = index % width;
        let row = index / width;
        EnuVector3::new(
            axis_cell_center(self.min_enu[0], self.max_enu[0], self.spacing_m, column),
            axis_cell_center(self.min_enu[1], self.max_enu[1], self.spacing_m, row),
            self.listener_height_m,
        )
    }
}

fn axis_count(minimum: f32, maximum: f32, spacing: f32) -> usize {
    ((maximum - minimum) / spacing).ceil().max(1.0) as usize
}

fn axis_cell_center(minimum: f32, maximum: f32, spacing: f32, index: usize) -> f32 {
    let lower = minimum + index as f32 * spacing;
    let upper = (lower + spacing).min(maximum);
    (lower + upper) * 0.5
}

/// Builds a derived cell and applies all classes that do not require neighbours.
#[must_use]
pub fn classify_sample_at_distance(
    raw: AnomalyRawSample,
    source_spl_at_one_meter_db: f32,
    distance_m: f32,
) -> ProxyCell {
    let mean_eq_squared = raw
        .path_eq
        .into_iter()
        .map(|value| value * value)
        .sum::<f32>()
        / 3.0;
    let path_send_energy = raw.path_sh_energy * mean_eq_squared;
    let direct_loss_db = if raw.direct_audibility.is_finite() {
        -20.0 * raw.direct_audibility.max(ENERGY_FLOOR).log10()
    } else {
        f32::NAN
    };
    let path_strength_db = if path_send_energy.is_finite() {
        10.0 * path_send_energy.max(ENERGY_FLOOR).log10()
    } else {
        f32::NAN
    };
    let free_field_db = source_spl_at_one_meter_db - 20.0 * distance_m.max(SOURCE_RADIUS_M).log10();
    let shadow = ((direct_loss_db - MIN_INVERSION_DIRECT_LOSS_DB) / 24.0).clamp(0.0, 1.0);
    let weak_path =
        ((MAX_INVERSION_PATH_STRENGTH_DB - path_strength_db) / 40.0 + 0.5).clamp(0.0, 1.0);
    let meaningful = ((free_field_db - MIN_MEANINGFUL_FREE_FIELD_DB) / 20.0).clamp(0.0, 1.0);
    let score = shadow * weak_path * meaningful;
    let reflection_excess_db = raw
        .reflection_energy
        .zip(raw.direct_path_energy)
        .filter(|(reflection, direct_path)| {
            reflection.is_finite() && direct_path.is_finite() && *direct_path > 0.0
        })
        .map(|(reflection, direct_path)| {
            (10.0 * (reflection / direct_path.max(f64::from(ENERGY_FLOOR))).log10()) as f32
        });
    let mut flags = AnomalyFlags::default();
    if free_field_db >= MIN_MEANINGFUL_FREE_FIELD_DB
        && direct_loss_db >= MIN_INVERSION_DIRECT_LOSS_DB
        && path_strength_db <= MAX_INVERSION_PATH_STRENGTH_DB
    {
        flags.insert(AnomalyClass::InversionSignature);
    }
    let energies = [
        f64::from(raw.path_sh_energy),
        f64::from(path_send_energy),
        raw.direct_path_energy.unwrap_or(0.0),
        raw.reflection_energy.unwrap_or(0.0),
    ];
    if energies.into_iter().any(invalid_energy) {
        flags.insert(AnomalyClass::InvalidEnergy);
    }
    if !in_unit_interval(raw.direct_audibility)
        || raw
            .path_eq
            .into_iter()
            .any(|value| !in_unit_interval(value))
        || !in_unit_interval(raw.path_coefficient_min)
        || !in_unit_interval(raw.path_coefficient_max)
    {
        flags.insert(AnomalyClass::InvalidCoefficient);
    }
    if raw.path_sh_energy == 0.0
        && raw.source_probe_covered
        && raw.listener_probe_covered
        && !raw.source_endpoint_inside_static_geometry
        && !raw.listener_endpoint_inside_static_geometry
    {
        flags.insert(AnomalyClass::ZeroPathWithCoverage);
    }
    if reflection_excess_db.is_some_and(|value| value > MAX_PLAUSIBLE_REFLECTION_EXCESS_DB) {
        flags.insert(AnomalyClass::ReflectionEnergyExcess);
    }
    ProxyCell {
        position_enu: raw.position_enu,
        direct_audibility: raw.direct_audibility,
        direct_loss_db,
        path_sh_energy: raw.path_sh_energy,
        path_strength_db,
        free_field_db,
        score,
        source_probe_covered: raw.source_probe_covered,
        listener_probe_covered: raw.listener_probe_covered,
        direct_path_energy: raw.direct_path_energy,
        reflection_energy: raw.reflection_energy,
        reflection_excess_db,
        flags,
    }
}

fn invalid_energy(value: f64) -> bool {
    !value.is_finite() || (value != 0.0 && value.abs() < f64::from(f32::MIN_POSITIVE))
}

fn in_unit_interval(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

/// Applies spike and slope classes in-place after the row-major field is complete.
pub fn classify_grid(cells: &mut [ProxyCell], width: usize, height: usize, spacing_m: f32) {
    if width == 0 || height == 0 || width.saturating_mul(height) != cells.len() {
        return;
    }
    for cell in &mut *cells {
        cell.flags.remove(AnomalyClass::NeighborSpike);
        cell.flags.remove(AnomalyClass::ExcessiveDiscontinuity);
    }
    let mut spike = vec![false; cells.len()];
    let mut discontinuity = vec![false; cells.len()];
    for row in 0..height {
        for column in 0..width {
            let index = row * width + column;
            let neighbours = cardinal_neighbours(column, row, width, height);
            let valid = neighbours
                .into_iter()
                .flatten()
                .filter(|&other| {
                    cells[other].direct_audibility.is_finite()
                        && cells[other].path_strength_db.is_finite()
                })
                .collect::<Vec<_>>();
            if valid.len() >= 3 {
                let occlusion_spike = valid.iter().all(|&other| {
                    (cells[index].direct_audibility - cells[other].direct_audibility).abs()
                        >= SINGLE_CELL_OCCLUSION_SPIKE_DELTA
                });
                let path_spike = valid.iter().all(|&other| {
                    (cells[index].path_strength_db - cells[other].path_strength_db).abs()
                        >= SINGLE_CELL_PATH_SPIKE_DB
                });
                spike[index] = occlusion_spike || path_spike;
            }
            for other in [
                (column + 1 < width).then_some(index + 1),
                (row + 1 < height).then_some(index + width),
            ]
            .into_iter()
            .flatten()
            {
                let slope = (cells[index].direct_audibility - cells[other].direct_audibility).abs()
                    / spacing_m.max(f32::MIN_POSITIVE);
                if slope > MAX_PLAUSIBLE_OCCLUSION_SLOPE_PER_M {
                    discontinuity[index] = true;
                    discontinuity[other] = true;
                }
            }
        }
    }
    for (index, cell) in cells.iter_mut().enumerate() {
        if spike[index] {
            cell.flags.insert(AnomalyClass::NeighborSpike);
        }
        if discontinuity[index] {
            cell.flags.insert(AnomalyClass::ExcessiveDiscontinuity);
        }
    }
}

fn cardinal_neighbours(
    column: usize,
    row: usize,
    width: usize,
    height: usize,
) -> [Option<usize>; 4] {
    [
        (column > 0).then_some(row * width + column.saturating_sub(1)),
        (column + 1 < width).then_some(row * width + column + 1),
        (row > 0).then_some((row.saturating_sub(1)) * width + column),
        (row + 1 < height).then_some((row + 1) * width + column),
    ]
}

/// Detects a transition into decreasing obstruction (increasing SDK audibility).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IntersectionTrigger {
    previous: Option<([f32; 2], f32)>,
    previous_sign: i8,
    last_trigger: Option<[f32; 2]>,
}

impl IntersectionTrigger {
    #[must_use]
    pub fn observe(&mut self, position_enu: [f32; 2], direct_audibility: f32) -> bool {
        if !position_enu.into_iter().all(f32::is_finite) || !in_unit_interval(direct_audibility) {
            return false;
        }
        let obstruction = 1.0 - direct_audibility;
        let Some((previous_position, previous_obstruction)) =
            self.previous.replace((position_enu, obstruction))
        else {
            return false;
        };
        let distance = distance_2d(previous_position, position_enu);
        if distance <= f32::EPSILON {
            return false;
        }
        let derivative = (obstruction - previous_obstruction) / distance;
        let sign = if derivative < -OCCLUSION_DERIVATIVE_EPSILON_PER_M {
            -1
        } else if derivative > OCCLUSION_DERIVATIVE_EPSILON_PER_M {
            1
        } else {
            0
        };
        let entered_decrease = sign == -1 && self.previous_sign >= 0;
        if sign != 0 {
            self.previous_sign = sign;
        }
        let cooldown_clear = self
            .last_trigger
            .is_none_or(|last| distance_2d(last, position_enu) >= INTERSECTION_TRIGGER_COOLDOWN_M);
        if entered_decrease && cooldown_clear {
            self.last_trigger = Some(position_enu);
            true
        } else {
            false
        }
    }
}

fn distance_2d(left: [f32; 2], right: [f32; 2]) -> f32 {
    (left[0] - right[0]).hypot(left[1] - right[1])
}

/// Simulation-only retained Direct/Pathing session. It is neither `Send` nor callback-safe.
pub struct AnomalyQuerySession {
    #[cfg(feature = "linked-sdk")]
    inner: crate::linked::AnomalyQuerySimulation,
    source_position: EnuVector3,
    #[cfg(feature = "linked-sdk")]
    source_probe_covered: bool,
    #[cfg(feature = "linked-sdk")]
    probe_spheres: Vec<(EnuVector3, f32)>,
    #[cfg(feature = "linked-sdk")]
    source_endpoint_inside_static_geometry: bool,
    #[cfg(feature = "linked-sdk")]
    static_geometry_endpoints: StaticGeometryEndpoints,
    #[cfg(not(feature = "linked-sdk"))]
    _private: (),
}

impl AnomalyQuerySession {
    pub fn new(
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
        mut simulation: S3SimulationConfig,
        source: MultiSourceDescriptor,
    ) -> Result<Self, BackendError> {
        simulation.direct_occlusion = crate::DirectOcclusionMode::Raycast;
        simulation.trace_path_validation = false;
        #[cfg(feature = "linked-sdk")]
        {
            let source_position = EnuVector3::new(
                source.initial_position_enu.east_m,
                source.initial_position_enu.north_m,
                source.initial_position_enu.up_m,
            );
            let coverage = baked.probe_coverage()?;
            let probe_spheres = coverage.spheres().collect::<Vec<_>>();
            let source_probe_covered = covered_by_spheres(&probe_spheres, source_position);
            let static_geometry_endpoints = StaticGeometryEndpoints::new(mesh);
            let source_endpoint_inside_static_geometry =
                static_geometry_endpoints.contains(source_position);
            let inner = crate::linked::build_anomaly_query_simulation(
                mesh,
                baked,
                AudioConfig {
                    sample_rate_hz: 48_000,
                    frame_size: 128,
                },
                simulation,
                source,
            )?;
            Ok(Self {
                inner,
                source_position,
                source_probe_covered,
                probe_spheres,
                source_endpoint_inside_static_geometry,
                static_geometry_endpoints,
            })
        }
        #[cfg(not(feature = "linked-sdk"))]
        {
            let _ = (mesh, baked, simulation, source);
            Err(BackendError::SdkUnavailable(crate::unavailable_metadata()))
        }
    }

    pub fn sample(&mut self, listener: EnuVector3) -> Result<AnomalyRawSample, BackendError> {
        #[cfg(feature = "linked-sdk")]
        {
            let diagnostics = self.inner.sample(listener)?;
            return Ok(AnomalyRawSample {
                position_enu: listener,
                direct_audibility: diagnostics.occlusion,
                path_eq: diagnostics.path_eq,
                path_sh_energy: diagnostics.path_sh_energy,
                path_coefficient_min: diagnostics
                    .path_eq
                    .into_iter()
                    .fold(f32::INFINITY, f32::min),
                path_coefficient_max: diagnostics
                    .path_eq
                    .into_iter()
                    .fold(f32::NEG_INFINITY, f32::max),
                source_probe_covered: self.source_probe_covered,
                listener_probe_covered: covered_by_spheres(&self.probe_spheres, listener),
                source_endpoint_inside_static_geometry: self.source_endpoint_inside_static_geometry,
                listener_endpoint_inside_static_geometry: self
                    .static_geometry_endpoints
                    .contains(listener),
                direct_path_energy: None,
                reflection_energy: None,
            });
        }
        #[cfg(not(feature = "linked-sdk"))]
        {
            let _ = listener;
            Err(BackendError::SdkUnavailable(crate::unavailable_metadata()))
        }
    }

    #[must_use]
    pub fn source_distance_m(&self, listener: EnuVector3) -> f32 {
        let dx = self.source_position.x - listener.x;
        let dy = self.source_position.y - listener.y;
        let dz = self.source_position.z - listener.z;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
}

#[cfg(feature = "linked-sdk")]
struct StaticGeometryEndpoints {
    projected_surfaces: Vec<ProjectedSurface>,
}

#[cfg(feature = "linked-sdk")]
impl StaticGeometryEndpoints {
    fn new(mesh: &SceneMesh) -> Self {
        let projected_surfaces = mesh
            .triangles
            .iter()
            .filter_map(|indices| {
                let [a, b, c] = indices.map(|index| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| mesh.vertices_enu_m.get(index))
                        .copied()
                });
                ProjectedSurface::new(a?, b?, c?)
            })
            .collect();
        Self { projected_surfaces }
    }

    fn contains(&self, position: EnuVector3) -> bool {
        self.projected_surfaces.iter().any(|surface| {
            surface
                .height_at(position.x, position.y)
                .is_some_and(|height| height > position.z + STATIC_GEOMETRY_ENDPOINT_EPSILON_M)
        })
    }
}

#[cfg(feature = "linked-sdk")]
const STATIC_GEOMETRY_ENDPOINT_EPSILON_M: f32 = 1.0e-3;

#[cfg(feature = "linked-sdk")]
struct ProjectedSurface {
    a: EnuVector3,
    b: EnuVector3,
    c: EnuVector3,
    denominator: f32,
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

#[cfg(feature = "linked-sdk")]
impl ProjectedSurface {
    fn new(a: EnuVector3, b: EnuVector3, c: EnuVector3) -> Option<Self> {
        let denominator = (b.y - c.y) * (a.x - c.x) + (c.x - b.x) * (a.y - c.y);
        if !denominator.is_finite() || denominator.abs() <= f32::EPSILON {
            return None;
        }
        Some(Self {
            a,
            b,
            c,
            denominator,
            min_x: a.x.min(b.x).min(c.x),
            max_x: a.x.max(b.x).max(c.x),
            min_y: a.y.min(b.y).min(c.y),
            max_y: a.y.max(b.y).max(c.y),
        })
    }

    fn height_at(&self, x: f32, y: f32) -> Option<f32> {
        if x < self.min_x - STATIC_GEOMETRY_ENDPOINT_EPSILON_M
            || x > self.max_x + STATIC_GEOMETRY_ENDPOINT_EPSILON_M
            || y < self.min_y - STATIC_GEOMETRY_ENDPOINT_EPSILON_M
            || y > self.max_y + STATIC_GEOMETRY_ENDPOINT_EPSILON_M
        {
            return None;
        }
        let a_weight = ((self.b.y - self.c.y) * (x - self.c.x)
            + (self.c.x - self.b.x) * (y - self.c.y))
            / self.denominator;
        let b_weight = ((self.c.y - self.a.y) * (x - self.c.x)
            + (self.a.x - self.c.x) * (y - self.c.y))
            / self.denominator;
        let c_weight = 1.0 - a_weight - b_weight;
        if a_weight < -STATIC_GEOMETRY_ENDPOINT_EPSILON_M
            || b_weight < -STATIC_GEOMETRY_ENDPOINT_EPSILON_M
            || c_weight < -STATIC_GEOMETRY_ENDPOINT_EPSILON_M
        {
            return None;
        }
        Some(a_weight * self.a.z + b_weight * self.b.z + c_weight * self.c.z)
    }
}

#[cfg(feature = "linked-sdk")]
fn covered_by_spheres(spheres: &[(EnuVector3, f32)], position: EnuVector3) -> bool {
    spheres.iter().any(|(center, radius)| {
        let dx = center.x - position.x;
        let dy = center.y - position.y;
        let dz = center.z - position.z;
        dx * dx + dy * dy + dz * dz <= radius * radius
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_raw() -> AnomalyRawSample {
        AnomalyRawSample {
            position_enu: EnuVector3::new(10.0, 0.0, 1.5),
            direct_audibility: 0.8,
            path_eq: [0.5; 3],
            path_sh_energy: 0.1,
            path_coefficient_min: 0.1,
            path_coefficient_max: 0.5,
            source_probe_covered: true,
            listener_probe_covered: true,
            source_endpoint_inside_static_geometry: false,
            listener_endpoint_inside_static_geometry: false,
            direct_path_energy: Some(1.0),
            reflection_energy: Some(1.0),
        }
    }

    fn classified(raw: AnomalyRawSample) -> ProxyCell {
        classify_sample_at_distance(raw, 105.0, 10.0)
    }

    #[test]
    fn local_anomaly_classes_each_have_a_positive_and_clean_negative() {
        let clean = classified(clean_raw());
        for class in [
            AnomalyClass::InversionSignature,
            AnomalyClass::InvalidEnergy,
            AnomalyClass::InvalidCoefficient,
            AnomalyClass::ZeroPathWithCoverage,
            AnomalyClass::ReflectionEnergyExcess,
        ] {
            assert!(
                !clean.flags.contains(class),
                "clean negative fired {}",
                class.id()
            );
        }

        let mut inversion = clean_raw();
        inversion.direct_audibility = 0.01;
        inversion.path_sh_energy = 1.0e-6;
        assert!(
            classified(inversion)
                .flags
                .contains(AnomalyClass::InversionSignature)
        );

        let mut invalid_energy = clean_raw();
        invalid_energy.path_sh_energy = f32::NAN;
        assert!(
            classified(invalid_energy)
                .flags
                .contains(AnomalyClass::InvalidEnergy)
        );
        let mut denormal_energy = clean_raw();
        denormal_energy.path_sh_energy = f32::from_bits(1);
        assert!(
            classified(denormal_energy)
                .flags
                .contains(AnomalyClass::InvalidEnergy)
        );

        let mut invalid_coefficient = clean_raw();
        invalid_coefficient.path_eq[1] = 1.01;
        assert!(
            classified(invalid_coefficient)
                .flags
                .contains(AnomalyClass::InvalidCoefficient)
        );

        let mut zero_path = clean_raw();
        zero_path.path_sh_energy = 0.0;
        assert!(
            classified(zero_path)
                .flags
                .contains(AnomalyClass::ZeroPathWithCoverage)
        );
        zero_path.listener_endpoint_inside_static_geometry = true;
        assert!(
            !classified(zero_path)
                .flags
                .contains(AnomalyClass::ZeroPathWithCoverage)
        );
        zero_path.listener_endpoint_inside_static_geometry = false;
        zero_path.source_endpoint_inside_static_geometry = true;
        assert!(
            !classified(zero_path)
                .flags
                .contains(AnomalyClass::ZeroPathWithCoverage)
        );

        let mut reflection = clean_raw();
        reflection.reflection_energy = Some(100.0);
        assert!(
            classified(reflection)
                .flags
                .contains(AnomalyClass::ReflectionEnergyExcess)
        );
    }

    #[test]
    fn grid_math_centres_partial_edge_cells() {
        let grid = GridSpec {
            min_enu: [0.0, 0.0],
            max_enu: [585.0, 585.0],
            listener_height_m: 1.5,
            spacing_m: 8.0,
        }
        .validate()
        .unwrap();
        assert_eq!(
            (grid.width(), grid.height(), grid.cell_count()),
            (74, 74, 5_476)
        );
        assert_eq!(grid.position(0), EnuVector3::new(4.0, 4.0, 1.5));
        assert_eq!(
            grid.position(grid.cell_count() - 1),
            EnuVector3::new(584.5, 584.5, 1.5)
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn static_geometry_endpoint_test_distinguishes_under_roof_outside_and_above() {
        let mesh = SceneMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, 0.0, 10.0),
                EnuVector3::new(10.0, 0.0, 10.0),
                EnuVector3::new(10.0, 10.0, 10.0),
                EnuVector3::new(0.0, 10.0, 10.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            material_indices: vec![0, 0],
            materials: vec![],
        };
        let endpoints = StaticGeometryEndpoints::new(&mesh);

        assert!(endpoints.contains(EnuVector3::new(5.0, 5.0, 1.5)));
        assert!(!endpoints.contains(EnuVector3::new(11.0, 5.0, 1.5)));
        assert!(!endpoints.contains(EnuVector3::new(5.0, 5.0, 10.0)));
        assert!(!endpoints.contains(EnuVector3::new(5.0, 5.0, 11.0)));
    }

    #[test]
    fn neighbour_classes_have_synthetic_positives_and_clean_negatives() {
        let mut cells = vec![classified(clean_raw()); 9];
        for (index, cell) in cells.iter_mut().enumerate() {
            cell.position_enu.x = (index % 3) as f32;
            cell.position_enu.y = (index / 3) as f32;
        }
        classify_grid(&mut cells, 3, 3, 4.0);
        assert!(!cells[4].flags.contains(AnomalyClass::NeighborSpike));
        assert!(
            !cells[4]
                .flags
                .contains(AnomalyClass::ExcessiveDiscontinuity)
        );

        cells[4].direct_audibility = 0.0;
        classify_grid(&mut cells, 3, 3, 4.0);
        assert!(cells[4].flags.contains(AnomalyClass::NeighborSpike));
        assert!(
            !cells[4]
                .flags
                .contains(AnomalyClass::ExcessiveDiscontinuity)
        );

        classify_grid(&mut cells, 3, 3, 1.0);
        assert!(
            cells[4]
                .flags
                .contains(AnomalyClass::ExcessiveDiscontinuity)
        );
        assert!(
            cells[1]
                .flags
                .contains(AnomalyClass::ExcessiveDiscontinuity)
        );
    }

    #[test]
    fn occlusion_derivative_sign_change_triggers_one_bounded_dense_area() {
        let mut trigger = IntersectionTrigger::default();
        let path = [
            ([0.0, 0.0], 0.10),
            ([1.0, 0.0], 0.10),
            ([2.0, 0.0], 0.30),
            ([3.0, 0.0], 0.50),
            ([4.0, 0.0], 0.70),
        ];
        let fired = path
            .into_iter()
            .filter_map(|(position, occlusion)| {
                trigger.observe(position, occlusion).then_some(position)
            })
            .collect::<Vec<_>>();
        assert_eq!(fired, vec![[2.0, 0.0]]);
    }
}
