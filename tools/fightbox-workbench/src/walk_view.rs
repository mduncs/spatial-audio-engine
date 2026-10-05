//! Phone-first walking views, prototyped in the desktop Workbench: sounds
//! pinned to real places, their physical size and open-air reach, and light
//! street names for orientation. Display only; nothing here feeds acoustics.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::ground_map::{GroundMap, Point};

/// Which prototype layout draws the listening view (`FIGHTBOX_WALK_VIEW`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkDesign {
    /// A: heading-up map that follows you, with a bottom sheet.
    Map,
    /// B: first-person street with corner signs and a mini-map.
    Walk,
    /// C: everything within earshot on one fisheye radar, sorted by loudness.
    Earshot,
}

impl WalkDesign {
    pub(crate) const ALL: [Self; 3] = [Self::Map, Self::Walk, Self::Earshot];

    pub(crate) fn from_env() -> Option<Self> {
        Self::parse(&std::env::var("FIGHTBOX_WALK_VIEW").ok()?)
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "a" | "map" => Some(Self::Map),
            "b" | "walk" => Some(Self::Walk),
            "c" | "earshot" => Some(Self::Earshot),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) const fn letter(self) -> &'static str {
        match self {
            Self::Map => "A",
            Self::Walk => "B",
            Self::Earshot => "C",
        }
    }
}

/// Per-scene walk-view state. `design` is `None` unless the env switch is set,
/// in which case the classic listening view is unchanged.
pub(crate) struct WalkUi {
    pub design: Option<WalkDesign>,
    pub atlas: StreetAtlas,
    /// Size preset per source; `None` keeps the scene's authored size.
    pub presets: Vec<Option<usize>>,
    /// Authored line or stereo width per source, in metres (0 for a point).
    pub authored_width_m: Vec<f32>,
    pub ground_up_m: f32,
    pub placing: Option<Point>,
}

// ---------------------------------------------------------------- size

pub(crate) struct SizePreset {
    pub label: &'static str,
    pub spl_at_one_m_db: f32,
    pub width_m: f32,
}

/// Loudness and physical width move together; md can rename or re-level.
pub(crate) const SIZE_PRESETS: [SizePreset; 5] = [
    SizePreset {
        label: "Phone",
        spl_at_one_m_db: 80.0,
        width_m: 0.1,
    },
    SizePreset {
        label: "Boombox",
        spl_at_one_m_db: 95.0,
        width_m: 0.6,
    },
    SizePreset {
        label: "Party PA",
        spl_at_one_m_db: 115.0,
        width_m: 4.0,
    },
    SizePreset {
        label: "Church bell",
        spl_at_one_m_db: 125.0,
        width_m: 2.0,
    },
    SizePreset {
        label: "Stadium",
        spl_at_one_m_db: 135.0,
        width_m: 30.0,
    },
];

/// A quiet city street. "Carries" means still above this in open air.
pub(crate) const STREET_AMBIENT_DB_SPL: f32 = 55.0;

/// The preset an authored level already matches (within 1 dB). Any other
/// level stays as authored rather than being snapped to a nearby preset.
pub(crate) fn matching_preset(spl_at_one_m_db: f32) -> Option<usize> {
    SIZE_PRESETS
        .iter()
        .position(|preset| (preset.spl_at_one_m_db - spl_at_one_m_db).abs() <= 1.0)
}

/// Open-air level from spherical spreading plus mid-band air absorption.
/// Buildings, ground and reflections are deliberately absent.
pub(crate) fn open_air_level_db(spl_at_one_m_db: f32, distance_m: f32, air_db_per_m: f32) -> f32 {
    let distance_m = distance_m.max(1.0);
    spl_at_one_m_db - 20.0 * distance_m.log10() - air_db_per_m * distance_m
}

/// Distance where the open-air level falls to a quiet street.
pub(crate) fn reach_m(spl_at_one_m_db: f32, air_db_per_m: f32) -> f32 {
    let above =
        |r: f32| open_air_level_db(spl_at_one_m_db, r, air_db_per_m) > STREET_AMBIENT_DB_SPL;
    if !above(1.0) {
        return 1.0;
    }
    let (mut low, mut high) = (1.0_f32, 50_000.0_f32);
    for _ in 0..48 {
        let middle = (low * high).sqrt();
        if above(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
}

pub(crate) fn format_distance(metres: f32) -> String {
    if metres >= 995.0 {
        format!("{:.1} km", metres / 1000.0)
    } else if metres >= 100.0 {
        format!("{:.0} m", (metres / 10.0).round() * 10.0)
    } else {
        format!("{:.0} m", metres.max(0.0))
    }
}

pub(crate) fn format_reach(metres: f32) -> String {
    format!("~{}", format_distance(metres))
}

/// Ground bearing relative to the listener's facing, in walking words.
pub(crate) fn relative_direction(listener: Point, yaw: f32, target: Point) -> &'static str {
    let bearing = (target[0] - listener[0]).atan2(target[1] - listener[1]);
    let degrees = (bearing - yaw).to_degrees().rem_euclid(360.0);
    match degrees {
        d if !(22.5..337.5).contains(&d) => "ahead",
        d if d < 67.5 => "ahead-right",
        d if d < 112.5 => "right",
        d if d < 157.5 => "behind-right",
        d if d < 202.5 => "behind",
        d if d < 247.5 => "behind-left",
        d if d < 292.5 => "left",
        _ => "ahead-left",
    }
}

pub(crate) fn compass_word(from: Point, to: Point) -> &'static str {
    let degrees = (to[0] - from[0])
        .atan2(to[1] - from[1])
        .to_degrees()
        .rem_euclid(360.0);
    ["N", "NE", "E", "SE", "S", "SW", "W", "NW"][((degrees + 22.5) / 45.0) as usize % 8]
}

// ---------------------------------------------------------------- streets

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StreetKind {
    Path,
    Alley,
    Road,
    Major,
}

impl StreetKind {
    pub(crate) fn from_osm(highway: &str, named: bool) -> Self {
        match highway.trim_end_matches("_link") {
            "motorway" | "trunk" | "primary" | "secondary" | "tertiary" => Self::Major,
            "footway" | "path" | "steps" | "cycleway" | "pedestrian" | "track" | "bridleway"
            | "corridor" => Self::Path,
            "service" => Self::Alley,
            "" if !named => Self::Path,
            _ => Self::Road,
        }
    }

    /// Typical paved width, so roads read at true scale on a metric map.
    pub(crate) const fn width_m(self) -> f32 {
        match self {
            Self::Path => 1.5,
            Self::Alley => 5.0,
            Self::Road => 10.0,
            Self::Major => 14.0,
        }
    }

    const fn map_color(self) -> Color32 {
        match self {
            Self::Path => Color32::from_rgba_premultiplied(44, 50, 55, 150),
            Self::Alley => Color32::from_rgb(45, 53, 60),
            Self::Road => Color32::from_rgb(63, 73, 81),
            Self::Major => Color32::from_rgb(84, 93, 99),
        }
    }
}

/// US sign style: "North Clark Street" reads "N Clark St".
pub(crate) fn short_street_name(name: &str) -> String {
    let words = name.split_whitespace().collect::<Vec<_>>();
    let last = words.len().saturating_sub(1);
    words
        .iter()
        .enumerate()
        .map(|(index, word)| {
            let short = match (index, *word) {
                (0, "North") if last > 0 => "N",
                (0, "South") if last > 0 => "S",
                (0, "East") if last > 0 => "E",
                (0, "West") if last > 0 => "W",
                (i, "Avenue") if i == last && i > 0 => "Ave",
                (i, "Street") if i == last && i > 0 => "St",
                (i, "Boulevard") if i == last && i > 0 => "Blvd",
                (i, "Road") if i == last && i > 0 => "Rd",
                (i, "Drive") if i == last && i > 0 => "Dr",
                (i, "Place") if i == last && i > 0 => "Pl",
                (i, "Court") if i == last && i > 0 => "Ct",
                (i, "Lane") if i == last && i > 0 => "Ln",
                (i, "Parkway") if i == last && i > 0 => "Pkwy",
                (i, "Terrace") if i == last && i > 0 => "Ter",
                (i, "Highway") if i == last && i > 0 => "Hwy",
                _ => word,
            };
            short
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) struct Street {
    pub name: String,
    pub kind: StreetKind,
    pub points: Vec<Point>,
}

/// One straight stretch of a named street; labels and signs sit on these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NamedRun {
    pub name: usize,
    pub kind: StreetKind,
    pub a: Point,
    pub b: Point,
}

pub(crate) struct StreetAtlas {
    pub streets: Vec<Street>,
    pub names: Vec<String>,
    pub runs: Vec<NamedRun>,
    /// Where two differently named streets cross.
    pub corners: Vec<(Point, [usize; 2])>,
}

/// Where you are, in street words, for the top bar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlaceNote {
    pub here: String,
    pub near: String,
}

impl StreetAtlas {
    pub(crate) fn new(lines: &[Vec<Point>], names: &[String], kinds: &[String]) -> Self {
        let streets = lines
            .iter()
            .enumerate()
            .filter(|(_, points)| points.len() >= 2)
            .map(|(index, points)| {
                let name = names
                    .get(index)
                    .map(|name| short_street_name(name.trim()))
                    .unwrap_or_default();
                let kind = StreetKind::from_osm(
                    kinds.get(index).map_or("", String::as_str),
                    !name.is_empty(),
                );
                Street {
                    name,
                    kind,
                    points: points.clone(),
                }
            })
            .collect::<Vec<_>>();
        let mut names = Vec::<String>::new();
        let mut runs = Vec::new();
        for street in streets
            .iter()
            .filter(|street| !street.name.is_empty() && street.kind != StreetKind::Path)
        {
            let name = names
                .iter()
                .position(|name| *name == street.name)
                .unwrap_or_else(|| {
                    names.push(street.name.clone());
                    names.len() - 1
                });
            for pair in street.points.windows(2) {
                if distance(pair[0], pair[1]) > 0.01 {
                    runs.push(NamedRun {
                        name,
                        kind: street.kind,
                        a: pair[0],
                        b: pair[1],
                    });
                }
            }
        }
        let runs = merge_runs(runs);
        let mut corners = Vec::<(Point, [usize; 2])>::new();
        for (i, first) in runs.iter().enumerate() {
            for second in &runs[i + 1..] {
                if first.name == second.name {
                    continue;
                }
                let Some(point) = crossing(first, second) else {
                    continue;
                };
                let pair = [first.name.min(second.name), first.name.max(second.name)];
                if !corners
                    .iter()
                    .any(|(other, names)| *names == pair && distance(*other, point) < 40.0)
                {
                    corners.push((point, pair));
                }
            }
        }
        Self {
            streets,
            names,
            runs,
            corners,
        }
    }

    fn nearest_run(&self, point: Point, skip: Option<usize>) -> Option<(usize, f32, Point)> {
        self.runs
            .iter()
            .filter(|run| Some(run.name) != skip)
            .map(|run| {
                let closest = closest_on_segment(point, run.a, run.b);
                (run.name, distance(point, closest), closest)
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// The street name painted at a ground point: "N Clark St", or the
    /// nearest named street with distance.
    pub(crate) fn street_at(&self, point: Point) -> String {
        match self.nearest_run(point, None) {
            Some((name, metres, _)) if metres <= 12.0 => self.names[name].clone(),
            Some((name, metres, closest)) => format!(
                "{} {} of {}",
                format_distance(metres),
                compass_word(closest, point),
                self.names[name]
            ),
            None => "Unnamed spot".to_owned(),
        }
    }

    pub(crate) fn place_note(&self, point: Point) -> PlaceNote {
        let on_named = self
            .nearest_run(point, None)
            .filter(|(name, metres, _)| {
                let kind = self
                    .runs
                    .iter()
                    .filter(|run| run.name == *name)
                    .map(|run| run.kind)
                    .max()
                    .unwrap_or(StreetKind::Road);
                *metres <= kind.width_m() * 0.5 + 6.0
            })
            .map(|(name, _, _)| name);
        let in_alley = self.streets.iter().any(|street| {
            street.kind == StreetKind::Alley
                && street
                    .points
                    .windows(2)
                    .any(|pair| distance(point, closest_on_segment(point, pair[0], pair[1])) <= 4.0)
        });
        let here = match on_named {
            Some(name) => self.names[name].clone(),
            None if in_alley => "Alley".to_owned(),
            None if self.streets.is_empty() => "Street names unavailable".to_owned(),
            None => "Between streets".to_owned(),
        };
        // The two nearest other named streets, like a cross-street address.
        let mut nearest = Vec::<(usize, f32, Point)>::new();
        for run in self.runs.iter().filter(|run| Some(run.name) != on_named) {
            let closest = closest_on_segment(point, run.a, run.b);
            let metres = distance(point, closest);
            match nearest.iter_mut().find(|(name, _, _)| *name == run.name) {
                Some(entry) if metres < entry.1 => *entry = (run.name, metres, closest),
                Some(_) => {}
                None => nearest.push((run.name, metres, closest)),
            }
        }
        nearest.sort_by(|a, b| a.1.total_cmp(&b.1));
        let near = nearest
            .iter()
            .take(2)
            .map(|(name, metres, closest)| {
                format!(
                    "{} {} {}",
                    self.names[*name],
                    format_distance(*metres),
                    compass_word(point, *closest)
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        PlaceNote { here, near }
    }
}

fn merge_runs(mut runs: Vec<NamedRun>) -> Vec<NamedRun> {
    loop {
        let mut joined = None;
        'search: for i in 0..runs.len() {
            for j in i + 1..runs.len() {
                if runs[i].name == runs[j].name
                    && let Some(run) = join_runs(runs[i], runs[j])
                {
                    joined = Some((i, j, run));
                    break 'search;
                }
            }
        }
        let Some((i, j, run)) = joined else {
            return runs;
        };
        runs[i] = run;
        runs.swap_remove(j);
    }
}

fn join_runs(first: NamedRun, second: NamedRun) -> Option<NamedRun> {
    let unit = |run: NamedRun| {
        let length = distance(run.a, run.b).max(1.0e-6);
        [
            (run.b[0] - run.a[0]) / length,
            (run.b[1] - run.a[1]) / length,
        ]
    };
    let (u, v) = (unit(first), unit(second));
    // Within about 8 degrees, either way round.
    if (u[0] * v[1] - u[1] * v[0]).abs() > 0.14 {
        return None;
    }
    for (p_first, far_first) in [(first.a, first.b), (first.b, first.a)] {
        for (p_second, far_second) in [(second.a, second.b), (second.b, second.a)] {
            if distance(p_first, p_second) < 0.75 {
                return Some(NamedRun {
                    name: first.name,
                    kind: first.kind.max(second.kind),
                    a: far_first,
                    b: far_second,
                });
            }
        }
    }
    None
}

/// Crossing of two runs, each extended a little so T-junctions count.
fn crossing(first: &NamedRun, second: &NamedRun) -> Option<Point> {
    let extend = |run: &NamedRun| {
        let length = distance(run.a, run.b).max(1.0e-6);
        let d = [
            (run.b[0] - run.a[0]) / length,
            (run.b[1] - run.a[1]) / length,
        ];
        (
            [run.a[0] - d[0] * 3.0, run.a[1] - d[1] * 3.0],
            [run.b[0] + d[0] * 3.0, run.b[1] + d[1] * 3.0],
        )
    };
    let ((a, b), (c, d)) = (extend(first), extend(second));
    let r = [b[0] - a[0], b[1] - a[1]];
    let s = [d[0] - c[0], d[1] - c[1]];
    let denominator = r[0] * s[1] - r[1] * s[0];
    if denominator.abs() < 1.0e-6 {
        return None;
    }
    let t = ((c[0] - a[0]) * s[1] - (c[1] - a[1]) * s[0]) / denominator;
    let u = ((c[0] - a[0]) * r[1] - (c[1] - a[1]) * r[0]) / denominator;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u))
        .then(|| [a[0] + r[0] * t, a[1] + r[1] * t])
}

pub(crate) fn distance(a: Point, b: Point) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn closest_on_segment(point: Point, a: Point, b: Point) -> Point {
    let d = [b[0] - a[0], b[1] - a[1]];
    let length_squared = d[0] * d[0] + d[1] * d[1];
    let t = if length_squared <= 1.0e-12 {
        0.0
    } else {
        (((point[0] - a[0]) * d[0] + (point[1] - a[1]) * d[1]) / length_squared).clamp(0.0, 1.0)
    };
    [a[0] + d[0] * t, a[1] + d[1] * t]
}

fn lerp(a: Point, b: Point, t: f32) -> Point {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

// ---------------------------------------------------------------- plans

/// Heading-up plan view around the listener: your facing is screen-up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Plan {
    origin: Point,
    pub anchor: Pos2,
    yaw: f32,
    scale: PlanScale,
}

#[derive(Clone, Copy, Debug)]
enum PlanScale {
    Linear {
        px_per_m: f32,
    },
    /// Linear near you and logarithmic far away (asinh), so a phone speaker's
    /// reach and a stadium's both fit on one screen.
    Fisheye {
        radius_px: f32,
        knee_m: f32,
        max_m: f32,
    },
}

impl Plan {
    pub(crate) fn heading_up(origin: Point, yaw: f32, anchor: Pos2, px_per_m: f32) -> Self {
        Self {
            origin,
            anchor,
            yaw,
            scale: PlanScale::Linear { px_per_m },
        }
    }

    pub(crate) fn fisheye(
        origin: Point,
        yaw: f32,
        anchor: Pos2,
        radius_px: f32,
        knee_m: f32,
        max_m: f32,
    ) -> Self {
        Self {
            origin,
            anchor,
            yaw,
            scale: PlanScale::Fisheye {
                radius_px,
                knee_m,
                max_m,
            },
        }
    }

    fn local(self, point: Point) -> Vec2 {
        let d = [point[0] - self.origin[0], point[1] - self.origin[1]];
        let (sin, cos) = self.yaw.sin_cos();
        Vec2::new(d[0] * cos - d[1] * sin, -(d[0] * sin + d[1] * cos))
    }

    pub(crate) fn radius_px(self, metres: f32) -> f32 {
        match self.scale {
            PlanScale::Linear { px_per_m } => metres * px_per_m,
            PlanScale::Fisheye {
                radius_px,
                knee_m,
                max_m,
            } => radius_px * (metres / knee_m).asinh() / (max_m / knee_m).asinh(),
        }
    }

    pub(crate) fn metres_at(self, px: f32) -> f32 {
        match self.scale {
            PlanScale::Linear { px_per_m } => px / px_per_m,
            PlanScale::Fisheye {
                radius_px,
                knee_m,
                max_m,
            } => knee_m * (px / radius_px * (max_m / knee_m).asinh()).sinh(),
        }
    }

    pub(crate) fn px_per_m(self) -> Option<f32> {
        match self.scale {
            PlanScale::Linear { px_per_m } => Some(px_per_m),
            PlanScale::Fisheye { .. } => None,
        }
    }

    pub(crate) fn project(self, point: Point) -> Pos2 {
        let local = self.local(point);
        match self.scale {
            PlanScale::Linear { px_per_m } => self.anchor + local * px_per_m,
            PlanScale::Fisheye { .. } => {
                let metres = local.length();
                if metres < 1.0e-4 {
                    self.anchor
                } else {
                    self.anchor + local / metres * self.radius_px(metres)
                }
            }
        }
    }

    pub(crate) fn unproject(self, screen: Pos2) -> Point {
        let offset = screen - self.anchor;
        let px = offset.length();
        let local = if px < 1.0e-4 {
            Vec2::ZERO
        } else {
            offset / px * self.metres_at(px)
        };
        let (sin, cos) = self.yaw.sin_cos();
        let (right, forward) = (local.x, -local.y);
        [
            self.origin[0] + cos * right + sin * forward,
            self.origin[1] - sin * right + cos * forward,
        ]
    }

    /// Screen direction of true north.
    pub(crate) fn north(self) -> Vec2 {
        let (sin, cos) = self.yaw.sin_cos();
        Vec2::new(-sin, -cos)
    }

    fn polyline(self, points: &[Point]) -> Vec<Pos2> {
        let mut out = Vec::with_capacity(points.len());
        for (index, pair) in points.windows(2).enumerate() {
            if index == 0 {
                out.push(self.project(pair[0]));
            }
            let steps = match self.scale {
                PlanScale::Linear { .. } => 1,
                PlanScale::Fisheye { .. } => {
                    ((distance(pair[0], pair[1]) / 6.0).ceil() as usize).clamp(1, 64)
                }
            };
            for step in 1..=steps {
                out.push(self.project(lerp(pair[0], pair[1], step as f32 / steps as f32)));
            }
        }
        out
    }
}

// ---------------------------------------------------------------- palette

pub(crate) const BACKGROUND: Color32 = Color32::from_rgb(14, 18, 22);
const GROUND: Color32 = Color32::from_rgb(19, 24, 29);
const BUILDING: Color32 = Color32::from_rgb(39, 47, 54);
const BUILDING_EDGE: Color32 = Color32::from_rgb(62, 74, 82);
const LABEL: Color32 = Color32::from_rgb(214, 222, 226);
pub(crate) const YOU: Color32 = Color32::from_rgb(71, 220, 189);
pub(crate) const MUTED: Color32 = Color32::from_rgb(140, 152, 160);
const SIGN_GREEN: Color32 = Color32::from_rgb(18, 112, 74);

const PIN_COLORS: [Color32; 6] = [
    Color32::from_rgb(255, 184, 77),
    Color32::from_rgb(110, 190, 255),
    Color32::from_rgb(255, 117, 104),
    Color32::from_rgb(176, 222, 96),
    Color32::from_rgb(194, 156, 255),
    Color32::from_rgb(255, 146, 206),
];

pub(crate) fn pin_color(index: usize) -> Color32 {
    PIN_COLORS[index % PIN_COLORS.len()]
}

fn dim(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

// ---------------------------------------------------------------- pins

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SizeInfo {
    pub preset: Option<usize>,
    pub adjustable: bool,
    pub spl_at_one_m_db: f32,
    pub width_m: f32,
    pub reach_m: f32,
}

impl SizeInfo {
    pub(crate) fn label(self) -> String {
        match self.preset {
            Some(preset) => SIZE_PRESETS[preset].label.to_owned(),
            None => "Scene size".to_owned(),
        }
    }

    pub(crate) fn caption(self) -> String {
        format!(
            "{} · {} wide · {:.0} dB at 1 m · carries {} in open air",
            self.label(),
            if self.width_m < 0.5 {
                "point".to_owned()
            } else {
                format_distance(self.width_m)
            },
            self.spl_at_one_m_db,
            format_reach(self.reach_m)
        )
    }
}

pub(crate) struct Pin<'a> {
    pub index: usize,
    pub label: &'a str,
    pub position: [f32; 3],
    pub color: Color32,
    pub on: bool,
    pub selected: bool,
    pub moving: bool,
    pub size: SizeInfo,
    pub distance_m: f32,
    pub direction: &'static str,
    pub level_here_db: f32,
}

impl Pin<'_> {
    pub(crate) fn ground(&self) -> Point {
        [self.position[0], self.position[1]]
    }
}

/// Everything a walk design paints, gathered once per frame by the Workbench.
pub(crate) struct WalkFrame<'a> {
    pub map: &'a GroundMap,
    pub atlas: &'a StreetAtlas,
    pub listener: Point,
    pub yaw: f32,
    pub ground_up_m: f32,
    pub pins: Vec<Pin<'a>>,
    pub placing: Option<Placing>,
}

#[derive(Clone, Debug)]
pub(crate) struct Placing {
    pub at: Point,
    pub covered: bool,
    pub street: String,
    pub distance_m: f32,
    pub direction: &'static str,
}

// ---------------------------------------------------------------- city painter

/// Roads at true width, building footprints, then street names along roads.
pub(crate) fn paint_city(
    painter: &egui::Painter,
    rect: Rect,
    plan: Plan,
    frame: &WalkFrame<'_>,
    label_px: Option<f32>,
) {
    let painter = painter.with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, GROUND);
    let visible = rect.expand(60.0);
    let road_px = |kind: StreetKind| match plan.px_per_m() {
        Some(px_per_m) => (kind.width_m() * px_per_m).clamp(1.0, 34.0),
        None => match kind {
            StreetKind::Path => 0.8,
            StreetKind::Alley => 2.0,
            StreetKind::Road => 4.5,
            StreetKind::Major => 6.5,
        },
    };
    for kind in [
        StreetKind::Path,
        StreetKind::Alley,
        StreetKind::Road,
        StreetKind::Major,
    ] {
        let width = road_px(kind);
        let color = kind.map_color();
        for street in frame
            .atlas
            .streets
            .iter()
            .filter(|street| street.kind == kind)
        {
            let (low, high) = frame.map.bounds;
            if plan.px_per_m().is_none()
                && !street.points.iter().any(|point| {
                    (low[0] - 40.0..=high[0] + 40.0).contains(&point[0])
                        && (low[1] - 40.0..=high[1] + 40.0).contains(&point[1])
                })
            {
                continue;
            }
            let points = plan.polyline(&street.points);
            if !points.iter().any(|point| visible.contains(*point)) {
                continue;
            }
            if kind != StreetKind::Path && width > 3.0 {
                for point in [points[0], points[points.len() - 1]] {
                    painter.circle_filled(point, width * 0.5, color);
                }
            }
            painter.add(Shape::line(points, Stroke::new(width, color)));
        }
    }
    // Roofs as one flat mesh: no anti-aliasing seams between triangles.
    let mut roofs = egui::Mesh::default();
    for roof in &frame.map.roofs {
        let points = roof.map(|point| plan.project(point));
        if points.iter().all(|point| !visible.contains(*point)) {
            continue;
        }
        let first = roofs.vertices.len() as u32;
        for point in points {
            roofs.colored_vertex(point, BUILDING);
        }
        roofs.add_triangle(first, first + 1, first + 2);
    }
    painter.add(Shape::mesh(roofs));
    for &(a, b) in &frame.map.walls {
        let points = plan.polyline(&[a, b]);
        if points.iter().all(|point| !visible.contains(*point)) {
            continue;
        }
        painter.add(Shape::line(points, Stroke::new(1.0, BUILDING_EDGE)));
    }
    if let Some(label_px) = label_px {
        paint_street_labels(&painter, rect.shrink(8.0), plan, frame.atlas, label_px);
    }
}

fn paint_street_labels(
    painter: &egui::Painter,
    safe: Rect,
    plan: Plan,
    atlas: &StreetAtlas,
    font_px: f32,
) {
    let mut runs = atlas.runs.iter().collect::<Vec<_>>();
    runs.sort_by(|a, b| {
        b.kind
            .cmp(&a.kind)
            .then(distance(b.a, b.b).total_cmp(&distance(a.a, a.b)))
    });
    let mut placed: Vec<(usize, Vec<Pos2>)> = Vec::new();
    for run in runs {
        let text = atlas.names[run.name].clone();
        let galley = painter.layout_no_wrap(text, FontId::proportional(font_px), LABEL);
        let need = galley.size().x + 18.0;
        const SAMPLES: usize = 64;
        let screen = (0..=SAMPLES)
            .map(|i| plan.project(lerp(run.a, run.b, i as f32 / SAMPLES as f32)))
            .collect::<Vec<_>>();
        // Longest contiguous on-screen stretch, measured along the road.
        let mut best: Option<(usize, usize, f32)> = None;
        let mut start = None;
        let mut length = 0.0;
        for i in 0..=SAMPLES {
            if safe.contains(screen[i]) {
                match start {
                    None => {
                        start = Some(i);
                        length = 0.0;
                    }
                    Some(_) => length += screen[i].distance(screen[i - 1]),
                }
                if let Some(first) = start
                    && best.is_none_or(|(_, _, best_length)| length > best_length)
                {
                    best = Some((first, i, length));
                }
            } else {
                start = None;
            }
        }
        let Some((first, last, length)) = best else {
            continue;
        };
        if length < need {
            continue;
        }
        let count = if length > need * 3.0 + 420.0 { 2 } else { 1 };
        for slot in 0..count {
            let t = (first as f32
                + (last - first) as f32 * (slot as f32 + 1.0) / (count as f32 + 1.0))
                / SAMPLES as f32;
            let run_length = distance(run.a, run.b).max(1.0);
            let dt = (3.0 / run_length).min(0.02);
            let center = plan.project(lerp(run.a, run.b, t));
            let mut tangent = (plan.project(lerp(run.a, run.b, (t + dt).min(1.0)))
                - plan.project(lerp(run.a, run.b, (t - dt).max(0.0))))
            .normalized();
            if !tangent.is_finite() {
                continue;
            }
            if tangent.x < 0.0 {
                tangent = -tangent;
            }
            let half = galley.size().x * 0.5 + 6.0;
            let samples = (-3..=3)
                .map(|k| center + tangent * (half * k as f32 / 3.0))
                .collect::<Vec<_>>();
            let crowded = placed.iter().any(|(name, points)| {
                let gap = if *name == run.name {
                    220.0
                } else {
                    font_px * 1.5
                };
                points
                    .iter()
                    .any(|a| samples.iter().any(|b| a.distance(*b) < gap))
            });
            if crowded {
                continue;
            }
            let normal = Vec2::new(-tangent.y, tangent.x) * (galley.size().y * 0.5 + 1.5);
            let along = tangent * half;
            painter.add(Shape::convex_polygon(
                vec![
                    center - along - normal,
                    center + along - normal,
                    center + along + normal,
                    center - along + normal,
                ],
                Color32::from_rgba_unmultiplied(28, 34, 40, 225),
                Stroke::new(1.0, run.kind.map_color()),
            ));
            let angle = tangent.y.atan2(tangent.x);
            let rotation = egui::emath::Rot2::from_angle(angle);
            let position = center - rotation * (galley.size() * 0.5);
            painter.add(
                egui::epaint::TextShape::new(position, galley.clone(), LABEL).with_angle(angle),
            );
            placed.push((run.name, samples));
        }
    }
}

// ---------------------------------------------------------------- plan overlays

/// Dashed open-air reach ring, a true-scale footprint, and the pin itself.
/// Pin name tags, painted after every pin head so none hides under a pin.
#[derive(Default)]
pub(crate) struct PinLabels {
    rects: Vec<Rect>,
    shapes: Vec<Shape>,
}

impl PinLabels {
    /// Keeps tags off a spot, such as the you marker.
    pub(crate) fn reserve(&mut self, rect: Rect) {
        self.rects.push(rect);
    }

    /// Paints the tags and returns the space they took.
    pub(crate) fn paint(self, painter: &egui::Painter) -> Vec<Rect> {
        painter.extend(self.shapes);
        self.rects
    }
}

pub(crate) fn paint_plan_pin(
    painter: &egui::Painter,
    rect: Rect,
    plan: Plan,
    pin: &Pin<'_>,
    labels: &mut PinLabels,
) -> Option<Pos2> {
    let painter = painter.with_clip_rect(rect);
    let center = plan.project(pin.ground());
    if pin.selected || pin.on {
        let ring = (0..=96)
            .map(|step| {
                let angle = step as f32 / 96.0 * std::f32::consts::TAU;
                plan.project([
                    pin.position[0] + pin.size.reach_m * angle.cos(),
                    pin.position[1] + pin.size.reach_m * angle.sin(),
                ])
            })
            .collect::<Vec<_>>();
        let alpha = if pin.selected { 210 } else { 90 };
        painter.extend(Shape::dashed_line(
            &ring,
            Stroke::new(1.6, dim(pin.color, alpha)),
            7.0,
            5.0,
        ));
        if pin.selected
            && let Some(top) = ring
                .iter()
                .filter(|point| rect.shrink(18.0).contains(**point))
                .min_by(|a, b| a.y.total_cmp(&b.y))
        {
            painter.text(
                *top + Vec2::new(0.0, 4.0),
                Align2::CENTER_TOP,
                format!("{} carries {}", pin.label, format_reach(pin.size.reach_m)),
                FontId::proportional(11.0),
                dim(pin.color, 230),
            );
        }
    }
    if !rect.expand(12.0).contains(center) {
        return None;
    }
    if let Some(px_per_m) = plan.px_per_m() {
        let footprint = pin.size.width_m * 0.5 * px_per_m;
        if footprint >= 3.0 {
            painter.circle_filled(center, footprint, dim(pin.color, 55));
        }
    }
    if pin.on {
        for (radius, alpha) in [(16.0, 120), (22.0, 60)] {
            painter.circle_stroke(center, radius, Stroke::new(1.5, dim(pin.color, alpha)));
        }
    }
    painter.circle_filled(
        center,
        10.0,
        if pin.on {
            pin.color
        } else {
            Color32::from_rgb(28, 34, 40)
        },
    );
    painter.circle_stroke(center, 10.0, Stroke::new(2.0, pin.color));
    if pin.selected {
        painter.circle_stroke(center, 13.5, Stroke::new(2.0, Color32::WHITE));
    }
    paint_play_glyph(
        &painter,
        center,
        4.5,
        pin.on,
        if pin.on { BACKGROUND } else { pin.color },
    );
    let text = format!("{} · {}", pin.label, format_distance(pin.distance_m));
    let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
    let size = galley.size();
    let right = Rect::from_min_size(center + Vec2::new(16.0, -size.y * 0.5), size);
    let left = Rect::from_min_size(center + Vec2::new(-16.0 - size.x, -size.y * 0.5), size);
    let free = |label: &Rect| {
        rect.shrink(4.0).contains_rect(*label)
            && !labels
                .rects
                .iter()
                .any(|other| other.intersects(label.expand(3.0)))
    };
    let label = if free(&right) {
        Some(right)
    } else if free(&left) {
        Some(left)
    } else if pin.selected {
        Some(if rect.shrink(4.0).contains_rect(right) {
            right
        } else {
            left
        })
    } else {
        None
    };
    if let Some(label) = label {
        let pill = label.expand2(Vec2::new(5.0, 2.0));
        labels.shapes.push(Shape::rect_filled(
            pill,
            6.0,
            Color32::from_rgba_unmultiplied(12, 16, 20, 225),
        ));
        labels
            .shapes
            .push(Shape::galley(label.min, galley, Color32::WHITE));
        labels.rects.push(pill);
    }
    Some(center)
}

/// Play triangle when off (tap to play), sound bars when playing.
fn paint_play_glyph(painter: &egui::Painter, center: Pos2, size: f32, on: bool, color: Color32) {
    if on {
        for (dx, h) in [(-size * 0.7, 0.55), (0.0, 1.0), (size * 0.7, 0.75)] {
            painter.line_segment(
                [
                    center + Vec2::new(dx, size * h),
                    center + Vec2::new(dx, -size * h),
                ],
                Stroke::new(size * 0.38, color),
            );
        }
    } else {
        painter.add(Shape::convex_polygon(
            vec![
                center + Vec2::new(-size * 0.6, -size),
                center + Vec2::new(size, 0.0),
                center + Vec2::new(-size * 0.6, size),
            ],
            color,
            Stroke::NONE,
        ));
    }
}

/// An off-screen pin as a chip on the edge, pointing along its true bearing.
pub(crate) fn paint_edge_chip(
    painter: &egui::Painter,
    safe: Rect,
    from: Pos2,
    to: Pos2,
    text: String,
    color: Color32,
    placed: &mut Vec<Rect>,
) {
    let Some((edge, bearing)) = crate::ground_map::source_edge_marker(from, to, safe) else {
        return;
    };
    let galley = painter.layout_no_wrap(text, FontId::proportional(11.5), Color32::WHITE);
    let size = galley.size() + Vec2::new(26.0, 8.0);
    let chip = Rect::from_center_size(edge - bearing * (size.x.max(size.y) * 0.5), size);
    let chip = chip.translate(Vec2::new(
        (safe.left() - chip.left()).max(0.0) + (safe.right() - chip.right()).min(0.0),
        (safe.top() - chip.top()).max(0.0) + (safe.bottom() - chip.bottom()).min(0.0),
    ));
    // Slide along the edge in quarter-chip steps, alternating sides, past
    // everything already placed; stack inward from the edge if a row is full.
    let (along, inward) = if bearing.x.abs() >= bearing.y.abs() {
        (
            Vec2::new(0.0, size.y + 4.0),
            Vec2::new(-bearing.x.signum() * (size.x + 4.0), 0.0),
        )
    } else {
        (
            Vec2::new(size.x + 4.0, 0.0),
            Vec2::new(0.0, -bearing.y.signum() * (size.y + 4.0)),
        )
    };
    let chip = (0..3)
        .flat_map(|row| {
            (0..=24).map(move |slot| {
                let side = if slot % 2 == 0 { 1.0 } else { -1.0 };
                chip.translate(
                    inward * row as f32 + along * (side * (slot + 1) as f32 / 2.0).trunc() * 0.25,
                )
            })
        })
        .find(|candidate| {
            safe.expand(0.5).contains_rect(*candidate)
                && !placed.iter().any(|other| other.intersects(*candidate))
        })
        .unwrap_or(chip);
    placed.push(chip);
    painter.rect_filled(chip, 9.0, Color32::from_rgba_unmultiplied(12, 16, 20, 225));
    painter.rect_stroke(
        chip,
        9.0,
        Stroke::new(1.0, dim(color, 160)),
        egui::StrokeKind::Inside,
    );
    let arrow = chip.left_center() + Vec2::new(10.0, 0.0);
    painter.add(Shape::convex_polygon(
        vec![
            arrow + bearing * 5.0,
            arrow - bearing * 4.0 + Vec2::new(-bearing.y, bearing.x) * 4.0,
            arrow - bearing * 4.0 - Vec2::new(-bearing.y, bearing.x) * 4.0,
        ],
        color,
        Stroke::NONE,
    ));
    painter.galley(
        chip.left_top() + Vec2::new(20.0, 4.0),
        galley,
        Color32::WHITE,
    );
}

pub(crate) fn paint_you(painter: &egui::Painter, at: Pos2, cone_px: f32) {
    let half = 28.0_f32.to_radians();
    let tip = |angle: f32| at + Vec2::new(angle.sin(), -angle.cos()) * cone_px;
    painter.add(Shape::convex_polygon(
        vec![
            at,
            tip(-half),
            tip(-half * 0.5),
            tip(0.0),
            tip(half * 0.5),
            tip(half),
        ],
        dim(YOU, 46),
        Stroke::NONE,
    ));
    painter.circle_filled(at, 7.5, YOU);
    painter.circle_stroke(at, 7.5, Stroke::new(2.0, Color32::WHITE));
}

pub(crate) fn paint_compass(painter: &egui::Painter, center: Pos2, plan: Plan) {
    painter.circle_filled(
        center,
        15.0,
        Color32::from_rgba_unmultiplied(12, 16, 20, 220),
    );
    painter.circle_stroke(center, 15.0, Stroke::new(1.0, MUTED));
    let north = plan.north();
    let side = Vec2::new(-north.y, north.x);
    painter.add(Shape::convex_polygon(
        vec![
            center + north * 12.0,
            center + side * 4.0,
            center - side * 4.0,
        ],
        Color32::from_rgb(255, 110, 96),
        Stroke::NONE,
    ));
    painter.text(
        center - north * 6.0,
        Align2::CENTER_CENTER,
        "N",
        FontId::proportional(10.0),
        Color32::WHITE,
    );
}

pub(crate) fn paint_scale_bar(painter: &egui::Painter, left_bottom: Pos2, px_per_m: f32) -> Rect {
    let metres = [
        5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0, 250.0, 500.0, 1000.0, 2000.0,
    ]
    .into_iter()
    .filter(|metres| metres * px_per_m <= 110.0)
    .last()
    .unwrap_or(5.0_f32);
    let width = metres * px_per_m;
    let y = left_bottom.y - 6.0;
    let (a, b) = (
        Pos2::new(left_bottom.x, y),
        Pos2::new(left_bottom.x + width, y),
    );
    let panel = Rect::from_min_max(
        Pos2::new(a.x - 6.0, y - 20.0),
        Pos2::new(b.x + 6.0, y + 5.0),
    );
    painter.rect_filled(panel, 5.0, Color32::from_rgba_unmultiplied(12, 16, 20, 200));
    let stroke = Stroke::new(2.0, Color32::WHITE);
    painter.line_segment([a, b], stroke);
    for x in [a.x, b.x] {
        painter.line_segment([Pos2::new(x, y - 5.0), Pos2::new(x, y + 1.0)], stroke);
    }
    painter.text(
        Pos2::new((a.x + b.x) * 0.5, y - 4.0),
        Align2::CENTER_BOTTOM,
        format_distance(metres),
        FontId::proportional(11.0),
        Color32::WHITE,
    );
    panel
}

/// Tap target for a new spot: crosshair, street name and bake coverage.
pub(crate) fn paint_placing(painter: &egui::Painter, at: Pos2, placing: &Placing) {
    let color = if placing.covered {
        Color32::from_rgb(110, 230, 150)
    } else {
        Color32::from_rgb(255, 96, 96)
    };
    painter.circle_stroke(at, 14.0, Stroke::new(2.0, color));
    painter.circle_filled(at, 3.0, color);
    for direction in [Vec2::X, -Vec2::X, Vec2::Y, -Vec2::Y] {
        painter.line_segment(
            [at + direction * 8.0, at + direction * 20.0],
            Stroke::new(2.0, color),
        );
    }
    let text = format!(
        "{}\n{} {}{}",
        placing.street,
        format_distance(placing.distance_m),
        placing.direction,
        if placing.covered {
            ""
        } else {
            " · no bake here"
        }
    );
    let galley = painter.layout(text, FontId::proportional(12.0), Color32::WHITE, 220.0);
    let safe = painter.clip_rect().shrink(8.0);
    let mut position = at + Vec2::new(-galley.size().x * 0.5, -galley.size().y - 26.0);
    position.x = position.x.clamp(
        safe.left(),
        (safe.right() - galley.size().x).max(safe.left()),
    );
    position.y = position.y.max(safe.top());
    painter.rect_filled(
        Rect::from_min_size(position, galley.size()).expand2(Vec2::new(6.0, 3.0)),
        6.0,
        Color32::from_rgba_unmultiplied(12, 16, 20, 230),
    );
    painter.galley(position, galley, Color32::WHITE);
}

/// Fisheye distance guides: "how far is that", even when scale bends.
pub(crate) fn paint_range_rings(painter: &egui::Painter, plan: Plan, limit_px: f32) {
    for metres in [25.0, 100.0, 250.0, 500.0, 1000.0, 2000.0] {
        let radius = plan.radius_px(metres);
        if radius > limit_px + 1.0 {
            continue;
        }
        painter.circle_stroke(
            plan.anchor,
            radius,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(150, 170, 180, 60)),
        );
        let at = plan.anchor + Vec2::new(0.70710677, 0.70710677) * radius;
        painter.text(
            at,
            Align2::LEFT_TOP,
            format_distance(metres),
            FontId::proportional(10.5),
            Color32::from_rgba_unmultiplied(170, 190, 200, 200),
        );
    }
}

pub(crate) fn minimap_plan(rect: Rect, frame: &WalkFrame<'_>) -> Plan {
    Plan::heading_up(
        frame.listener,
        frame.yaw,
        rect.center() + Vec2::new(0.0, rect.height() * 0.18),
        rect.width() / 180.0,
    )
}

/// B: a small heading-up map in the corner of the street view.
pub(crate) fn paint_minimap(painter: &egui::Painter, rect: Rect, frame: &WalkFrame<'_>) {
    let plan = minimap_plan(rect, frame);
    painter.rect_filled(
        rect.expand(3.0),
        6.0,
        Color32::from_rgba_unmultiplied(8, 11, 14, 235),
    );
    paint_city(
        painter,
        rect,
        plan,
        frame,
        (rect.width() >= 180.0).then_some(9.5),
    );
    let clipped = painter.with_clip_rect(rect);
    for pin in &frame.pins {
        let point = plan.project(pin.ground());
        if rect.shrink(5.0).contains(point) {
            clipped.circle_filled(point, 5.0, if pin.on { pin.color } else { BACKGROUND });
            clipped.circle_stroke(point, 5.0, Stroke::new(1.5, pin.color));
        } else if let Some((edge, _)) =
            crate::ground_map::source_edge_marker(plan.anchor, point, rect.shrink(5.0))
        {
            clipped.circle_filled(edge, 3.5, pin.color);
        }
    }
    paint_you(&clipped, plan.anchor, 20.0);
    paint_compass(painter, rect.right_top() + Vec2::new(-18.0, 18.0), plan);
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, MUTED),
        egui::StrokeKind::Outside,
    );
}

// ---------------------------------------------------------------- first person (B)

/// Projections the Workbench supplies for the first-person view.
pub(crate) struct Eye<'a> {
    pub point: &'a dyn Fn([f32; 3]) -> Option<Pos2>,
    /// Near-clipped ground polygon to screen.
    pub polygon: &'a dyn Fn(&[[f32; 3]]) -> Option<Vec<Pos2>>,
}

/// Painted between the ground and the buildings, so buildings hide them.
pub(crate) fn paint_ground_decals(painter: &egui::Painter, frame: &WalkFrame<'_>, eye: &Eye<'_>) {
    let z = frame.ground_up_m + 0.02;
    let near = |point: Point| distance(point, frame.listener) < 420.0;
    for kind in [StreetKind::Alley, StreetKind::Road, StreetKind::Major] {
        let color = match kind {
            StreetKind::Major => Color32::from_rgb(40, 45, 49),
            StreetKind::Road => Color32::from_rgb(44, 49, 52),
            _ => Color32::from_rgb(50, 55, 56),
        };
        for street in frame
            .atlas
            .streets
            .iter()
            .filter(|street| street.kind == kind)
        {
            for pair in street.points.windows(2) {
                if !near(pair[0]) && !near(pair[1]) {
                    continue;
                }
                let length = distance(pair[0], pair[1]).max(1.0e-3);
                let half = kind.width_m() * 0.5;
                let side = [
                    -(pair[1][1] - pair[0][1]) / length * half,
                    (pair[1][0] - pair[0][0]) / length * half,
                ];
                let quad = [
                    [pair[0][0] + side[0], pair[0][1] + side[1], z],
                    [pair[1][0] + side[0], pair[1][1] + side[1], z],
                    [pair[1][0] - side[0], pair[1][1] - side[1], z],
                    [pair[0][0] - side[0], pair[0][1] - side[1], z],
                ];
                if let Some(points) = (eye.polygon)(&quad) {
                    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
                }
                if kind != StreetKind::Alley {
                    // Faint centre dashes give the street a direction.
                    let steps = (length / 6.0).floor() as usize;
                    for step in 0..steps {
                        let a = lerp(pair[0], pair[1], step as f32 / steps as f32);
                        let b = lerp(pair[0], pair[1], (step as f32 + 0.45) / steps as f32);
                        if distance(a, frame.listener) > 160.0 {
                            continue;
                        }
                        let w = [side[0] / half * 0.08, side[1] / half * 0.08];
                        let dash = [
                            [a[0] + w[0], a[1] + w[1], z + 0.01],
                            [b[0] + w[0], b[1] + w[1], z + 0.01],
                            [b[0] - w[0], b[1] - w[1], z + 0.01],
                            [a[0] - w[0], a[1] - w[1], z + 0.01],
                        ];
                        if let Some(points) = (eye.polygon)(&dash) {
                            painter.add(Shape::convex_polygon(
                                points,
                                Color32::from_rgb(150, 132, 72),
                                Stroke::NONE,
                            ));
                        }
                    }
                }
            }
        }
    }
    for pin in frame.pins.iter().filter(|pin| pin.selected) {
        // The reach ring, chalked on the ground around the selected sound.
        let ring = (0..=120)
            .map(|step| {
                let angle = step as f32 / 120.0 * std::f32::consts::TAU;
                [
                    pin.position[0] + pin.size.reach_m * angle.cos(),
                    pin.position[1] + pin.size.reach_m * angle.sin(),
                    z + 0.03,
                ]
            })
            .collect::<Vec<_>>();
        for pair in ring.windows(2) {
            let width = (pin.size.reach_m * 0.004).clamp(0.25, 2.0);
            let dx = pair[1][0] - pair[0][0];
            let dy = pair[1][1] - pair[0][1];
            let length = dx.hypot(dy).max(1.0e-3);
            let side = [-dy / length * width, dx / length * width];
            let band = [
                [pair[0][0] + side[0], pair[0][1] + side[1], z + 0.03],
                [pair[1][0] + side[0], pair[1][1] + side[1], z + 0.03],
                [pair[1][0] - side[0], pair[1][1] - side[1], z + 0.03],
                [pair[0][0] - side[0], pair[0][1] - side[1], z + 0.03],
            ];
            if let Some(points) = (eye.polygon)(&band) {
                painter.add(Shape::convex_polygon(
                    points,
                    dim(pin.color, 150),
                    Stroke::NONE,
                ));
            }
        }
        let footprint = (0..24)
            .map(|step| {
                let angle = step as f32 / 24.0 * std::f32::consts::TAU;
                let radius = (pin.size.width_m * 0.5).max(0.6);
                [
                    pin.position[0] + radius * angle.cos(),
                    pin.position[1] + radius * angle.sin(),
                    z + 0.04,
                ]
            })
            .collect::<Vec<_>>();
        if let Some(points) = (eye.polygon)(&footprint) {
            painter.add(Shape::convex_polygon(
                points,
                dim(pin.color, 90),
                Stroke::NONE,
            ));
        }
    }
}

/// Corner signs: green plates on a pole at the near corner, both names.
pub(crate) fn paint_corner_signs(
    painter: &egui::Painter,
    rect: Rect,
    frame: &WalkFrame<'_>,
    eye: &Eye<'_>,
) {
    let mut corners = frame
        .atlas
        .corners
        .iter()
        .filter_map(|(corner, names)| {
            let metres = distance(*corner, frame.listener);
            (metres < 190.0).then_some((*corner, *names, metres))
        })
        .collect::<Vec<_>>();
    corners.sort_by(|a, b| b.2.total_cmp(&a.2));
    for (corner, names, metres) in corners {
        // Stand the pole on the corner you are approaching from.
        let toward = [frame.listener[0] - corner[0], frame.listener[1] - corner[1]];
        let length = toward[0].hypot(toward[1]).max(1.0e-3);
        let offset = 8.0_f32.min(length * 0.5);
        let base = [
            corner[0] + toward[0] / length * offset,
            corner[1] + toward[1] / length * offset,
        ];
        let z = frame.ground_up_m;
        let (Some(foot), Some(top)) = (
            (eye.point)([base[0], base[1], z]),
            (eye.point)([base[0], base[1], z + 3.4]),
        ) else {
            continue;
        };
        if !rect.contains(top) || behind_walls(frame, base) {
            continue;
        }
        let font = (300.0 / metres.max(1.0)).clamp(9.0, 15.0);
        painter.line_segment(
            [foot, top],
            Stroke::new((font * 0.18).max(1.2), Color32::from_rgb(96, 104, 108)),
        );
        let mut y = top.y;
        for name in names {
            let galley = painter.layout_no_wrap(
                frame.atlas.names[name].clone(),
                FontId::proportional(font),
                Color32::WHITE,
            );
            let plate = Rect::from_min_size(
                Pos2::new(
                    top.x - galley.size().x * 0.5 - font * 0.45,
                    y - galley.size().y - font * 0.3,
                ),
                galley.size() + Vec2::new(font * 0.9, font * 0.3),
            );
            painter.rect_filled(plate, 2.0, SIGN_GREEN);
            painter.rect_stroke(
                plate,
                2.0,
                Stroke::new(1.0, Color32::from_rgb(220, 235, 225)),
                egui::StrokeKind::Inside,
            );
            painter.galley(
                plate.min + Vec2::new(font * 0.45, font * 0.15),
                galley,
                Color32::WHITE,
            );
            y = plate.top() - 2.0;
        }
    }
}

/// True when a building wall stands between you and a ground point.
fn behind_walls(frame: &WalkFrame<'_>, point: Point) -> bool {
    let (a, b) = (frame.listener, point);
    let side = |p: Point, q: Point, r: Point| {
        (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])
    };
    frame
        .map
        .walls
        .iter()
        .any(|&(c, d)| side(a, b, c) * side(a, b, d) < 0.0 && side(c, d, a) * side(c, d, b) < 0.0)
}

/// Pins standing in the street: a beam from the ground up to a name tag.
/// Returns tap targets.
pub(crate) fn paint_walk_pins(
    painter: &egui::Painter,
    rect: Rect,
    frame: &WalkFrame<'_>,
    eye: &Eye<'_>,
    eye_yaw_sin_cos: (f32, f32),
) -> Vec<(usize, Rect)> {
    let mut targets = Vec::new();
    let mut order = frame.pins.iter().collect::<Vec<_>>();
    order.sort_by(|a, b| b.distance_m.total_cmp(&a.distance_m));
    let mut edge_rows = [0.0_f32; 2];
    for pin in order {
        let z = frame.ground_up_m;
        let top_z = pin.position[2].max(z + 2.5);
        let base = (eye.point)([pin.position[0], pin.position[1], z]);
        let top = (eye.point)([pin.position[0], pin.position[1], top_z]);
        match top.filter(|top| rect.shrink(4.0).contains(*top)) {
            Some(top) => {
                if let Some(base) = base {
                    painter.line_segment([base, top], Stroke::new(2.0, dim(pin.color, 170)));
                }
                let font = if pin.selected { 13.0 } else { 11.5 };
                let text = format!("{} · {}", pin.label, format_distance(pin.distance_m));
                let galley =
                    painter.layout_no_wrap(text, FontId::proportional(font), Color32::WHITE);
                let tag = Rect::from_min_size(
                    top - Vec2::new(galley.size().x * 0.5 + 14.0, galley.size().y + 12.0),
                    galley.size() + Vec2::new(28.0, 8.0),
                );
                painter.rect_filled(tag, 10.0, Color32::from_rgba_unmultiplied(12, 16, 20, 225));
                painter.rect_stroke(
                    tag,
                    10.0,
                    Stroke::new(
                        if pin.selected { 2.0 } else { 1.0 },
                        if pin.selected {
                            Color32::WHITE
                        } else {
                            dim(pin.color, 200)
                        },
                    ),
                    egui::StrokeKind::Inside,
                );
                let dot = tag.left_center() + Vec2::new(11.0, 0.0);
                painter.circle_filled(
                    dot,
                    6.0,
                    if pin.on {
                        pin.color
                    } else {
                        Color32::from_rgb(28, 34, 40)
                    },
                );
                painter.circle_stroke(dot, 6.0, Stroke::new(1.5, pin.color));
                paint_play_glyph(
                    painter,
                    dot,
                    3.0,
                    pin.on,
                    if pin.on { BACKGROUND } else { pin.color },
                );
                painter.galley(
                    tag.left_top() + Vec2::new(22.0, 4.0),
                    galley,
                    Color32::WHITE,
                );
                targets.push((pin.index, tag));
            }
            None => {
                let east = pin.position[0] - frame.listener[0];
                let north = pin.position[1] - frame.listener[1];
                let (sin, cos) = eye_yaw_sin_cos;
                let right = east * cos - north * sin;
                let side = usize::from(right >= 0.0);
                let y = rect.top() + 70.0 + edge_rows[side];
                edge_rows[side] += 30.0;
                let text = format!("{} · {}", pin.label, format_distance(pin.distance_m));
                let from = Pos2::new(rect.center().x, y);
                let to = Pos2::new(
                    if side == 1 {
                        rect.right() + 100.0
                    } else {
                        rect.left() - 100.0
                    },
                    y,
                );
                paint_edge_chip(
                    painter,
                    rect.shrink(8.0),
                    from,
                    to,
                    text,
                    pin.color,
                    &mut Vec::new(),
                );
            }
        }
    }
    targets
}

// ---------------------------------------------------------------- controls

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum WalkAction {
    Select(usize),
    /// `on` is the current state; same contract as `toggle_sound`.
    Toggle {
        index: usize,
        on: bool,
    },
    Preset {
        index: usize,
        preset: Option<usize>,
    },
    CancelPlace,
    PinHere {
        index: usize,
        at: Point,
    },
    PlayAll,
    StopAll,
    Diagnostics,
}

pub(crate) fn top_bar(
    ui: &mut egui::Ui,
    rect: Rect,
    note: &PlaceNote,
    facing: &str,
    actions: &mut Vec<WalkAction>,
) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, BACKGROUND);
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, Color32::from_rgb(34, 42, 48)),
    );
    painter.circle_filled(rect.left_top() + Vec2::new(22.0, 22.0), 5.0, YOU);
    let title = painter.text(
        rect.left_top() + Vec2::new(34.0, 12.0),
        Align2::LEFT_TOP,
        &note.here,
        FontId::proportional(17.0),
        Color32::WHITE,
    );
    painter.text(
        title.right_bottom() + Vec2::new(8.0, -3.0),
        Align2::LEFT_BOTTOM,
        format!("facing {facing}"),
        FontId::proportional(11.5),
        MUTED,
    );
    let galley = painter.layout(
        note.near.clone(),
        FontId::proportional(11.5),
        MUTED,
        rect.width() - 110.0,
    );
    painter.galley(rect.left_top() + Vec2::new(16.0, 36.0), galley, MUTED);
    let button = Rect::from_min_size(
        rect.right_top() + Vec2::new(-92.0, 12.0),
        Vec2::new(80.0, 26.0),
    );
    if ui
        .put(
            button,
            egui::Button::new(egui::RichText::new("Diagnostics").size(11.0)),
        )
        .clicked()
    {
        actions.push(WalkAction::Diagnostics);
    }
}

/// One size chip: name over reach. Presets move loudness and width together.
fn size_chip(
    ui: &mut egui::Ui,
    width: f32,
    title: &str,
    detail: &str,
    selected: bool,
    color: Color32,
    enabled: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, 46.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let painter = ui.painter_at(rect);
    let fill = if selected {
        dim(color, 60)
    } else {
        Color32::from_rgb(26, 32, 38)
    };
    painter.rect_filled(rect, 10.0, fill);
    painter.rect_stroke(
        rect,
        10.0,
        Stroke::new(
            if selected { 2.0 } else { 1.0 },
            if selected {
                color
            } else {
                Color32::from_rgb(52, 62, 70)
            },
        ),
        egui::StrokeKind::Inside,
    );
    let text_color = if enabled { Color32::WHITE } else { MUTED };
    painter.text(
        rect.center_top() + Vec2::new(0.0, 8.0),
        Align2::CENTER_TOP,
        title,
        FontId::proportional(12.0),
        text_color,
    );
    painter.text(
        rect.center_bottom() - Vec2::new(0.0, 8.0),
        Align2::CENTER_BOTTOM,
        detail,
        FontId::proportional(10.5),
        if selected { color } else { MUTED },
    );
    enabled && response.clicked()
}

pub(crate) fn size_row(
    ui: &mut egui::Ui,
    pin: &Pin<'_>,
    air_db_per_m: f32,
    actions: &mut Vec<WalkAction>,
) {
    let gap = 6.0;
    let width = ((ui.available_width() - gap * 4.0) / 5.0).floor();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (index, preset) in SIZE_PRESETS.iter().enumerate() {
            let selected = pin.size.preset == Some(index);
            let reach = format_reach(reach_m(preset.spl_at_one_m_db, air_db_per_m));
            if size_chip(
                ui,
                width,
                preset.label,
                &reach,
                selected,
                pin.color,
                pin.size.adjustable,
            ) {
                actions.push(WalkAction::Preset {
                    index: pin.index,
                    preset: Some(index),
                });
            }
        }
    });
}

pub(crate) fn play_button(
    ui: &mut egui::Ui,
    pin: &Pin<'_>,
    enabled: bool,
    size: Vec2,
    actions: &mut Vec<WalkAction>,
) {
    let text = if pin.on { "■  Stop" } else { "▶  Play" };
    let button = egui::Button::new(egui::RichText::new(text).size(15.0).color(if pin.on {
        Color32::WHITE
    } else {
        BACKGROUND
    }))
    .fill(if pin.on {
        Color32::from_rgb(52, 60, 68)
    } else {
        pin.color
    })
    .corner_radius(size.y * 0.5)
    .min_size(size);
    if ui.add_enabled(enabled, button).clicked() {
        actions.push(WalkAction::Toggle {
            index: pin.index,
            on: pin.on,
        });
    }
}

pub(crate) fn pin_heading(ui: &mut egui::Ui, pin: &Pin<'_>) {
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 6.0, pin.color);
        ui.label(
            egui::RichText::new(pin.label)
                .size(19.0)
                .color(Color32::WHITE)
                .strong(),
        );
    });
    let state = if pin.on { "playing" } else { "off" };
    let motion = if pin.moving { "moving" } else { "pinned here" };
    ui.label(
        egui::RichText::new(format!(
            "{} {} · {motion} · {state}",
            format_distance(pin.distance_m),
            pin.direction
        ))
        .size(12.5)
        .color(MUTED),
    );
}

/// Every sound as a tappable pill: colour, name, distance.
pub(crate) fn sound_pills(ui: &mut egui::Ui, pins: &[Pin<'_>], actions: &mut Vec<WalkAction>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        for pin in pins {
            let text = format!("{}  {}", pin.label, format_distance(pin.distance_m));
            let galley =
                ui.painter()
                    .layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
            let size = galley.size() + Vec2::new(30.0, 12.0);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
            let painter = ui.painter_at(rect.expand(2.0));
            painter.rect_filled(
                rect,
                size.y * 0.5,
                if pin.selected {
                    dim(pin.color, 55)
                } else {
                    Color32::from_rgb(26, 32, 38)
                },
            );
            painter.rect_stroke(
                rect,
                size.y * 0.5,
                Stroke::new(
                    1.0,
                    if pin.selected {
                        pin.color
                    } else {
                        Color32::from_rgb(52, 62, 70)
                    },
                ),
                egui::StrokeKind::Inside,
            );
            let dot = rect.left_center() + Vec2::new(12.0, 0.0);
            if pin.on {
                painter.circle_filled(dot, 4.5, pin.color);
            } else {
                painter.circle_stroke(dot, 4.0, Stroke::new(1.5, pin.color));
            }
            painter.galley(
                rect.left_top() + Vec2::new(22.0, 6.0),
                galley,
                Color32::WHITE,
            );
            if response.clicked() {
                actions.push(WalkAction::Select(pin.index));
            }
        }
    });
}

/// After tapping an empty spot: which sound goes here, at what size.
pub(crate) fn placing_sheet(
    ui: &mut egui::Ui,
    frame: &WalkFrame<'_>,
    placing: &Placing,
    actions: &mut Vec<WalkAction>,
) {
    ui.label(
        egui::RichText::new("Put a sound here")
            .size(19.0)
            .color(Color32::WHITE)
            .strong(),
    );
    ui.label(
        egui::RichText::new(format!(
            "{} · {} {}",
            placing.street,
            format_distance(placing.distance_m),
            placing.direction
        ))
        .size(12.5)
        .color(MUTED),
    );
    if !placing.covered {
        ui.label(
            egui::RichText::new(
                "Outside the baked area: sound here would be wrong, so pinning is off.",
            )
            .size(12.0)
            .color(Color32::from_rgb(255, 120, 110)),
        );
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        for pin in &frame.pins {
            let movable = !pin.moving && pin.size.adjustable;
            let button = egui::Button::new(
                egui::RichText::new(format!("Pin {} here", pin.label)).size(13.0),
            )
            .min_size(Vec2::new(0.0, 34.0))
            .corner_radius(17.0)
            .stroke(Stroke::new(1.0, dim(pin.color, 180)));
            let response = ui
                .add_enabled(movable && placing.covered, button)
                .on_disabled_hover_text(
                    "Moving or scene-authored sounds stay where the scene puts them",
                );
            if response.clicked() {
                actions.push(WalkAction::PinHere {
                    index: pin.index,
                    at: placing.at,
                });
            }
        }
    });
    ui.add_space(2.0);
    if ui
        .add(
            egui::Button::new(egui::RichText::new("Cancel").size(13.0))
                .min_size(Vec2::new(90.0, 32.0))
                .corner_radius(16.0),
        )
        .clicked()
    {
        actions.push(WalkAction::CancelPlace);
    }
}

/// C: every sound, loudest here first, with an open-air level bar.
pub(crate) fn loudness_list(
    ui: &mut egui::Ui,
    frame: &WalkFrame<'_>,
    can_listen: bool,
    air_db_per_m: f32,
    actions: &mut Vec<WalkAction>,
) {
    let mut order = frame.pins.iter().collect::<Vec<_>>();
    order.sort_by(|a, b| b.level_here_db.total_cmp(&a.level_here_db));
    for pin in order {
        let row_height = 50.0;
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(
            rect,
            12.0,
            if pin.selected {
                dim(pin.color, 34)
            } else {
                Color32::from_rgb(22, 28, 33)
            },
        );
        if pin.selected {
            painter.rect_stroke(
                rect,
                12.0,
                Stroke::new(1.5, dim(pin.color, 200)),
                egui::StrokeKind::Inside,
            );
        }
        let play =
            Rect::from_center_size(rect.left_center() + Vec2::new(26.0, 0.0), Vec2::splat(34.0));
        let play_response = ui.interact(
            play,
            ui.id().with(("walk-play", pin.index)),
            if can_listen {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        painter.circle_filled(
            play.center(),
            17.0,
            if pin.on {
                pin.color
            } else {
                Color32::from_rgb(34, 42, 48)
            },
        );
        painter.circle_stroke(play.center(), 17.0, Stroke::new(1.5, pin.color));
        paint_play_glyph(
            &painter,
            play.center(),
            6.0,
            pin.on,
            if pin.on { BACKGROUND } else { pin.color },
        );
        painter.text(
            rect.left_top() + Vec2::new(52.0, 8.0),
            Align2::LEFT_TOP,
            pin.label,
            FontId::proportional(14.5),
            Color32::WHITE,
        );
        // Arrow toward the sound, relative to your facing.
        let local = [
            pin.position[0] - frame.listener[0],
            pin.position[1] - frame.listener[1],
        ];
        let (sin, cos) = frame.yaw.sin_cos();
        let arrow = Vec2::new(
            local[0] * cos - local[1] * sin,
            -(local[0] * sin + local[1] * cos),
        )
        .normalized();
        let arrow_at = rect.left_top() + Vec2::new(58.0, 35.0);
        if arrow.is_finite() {
            painter.arrow(
                arrow_at - arrow * 5.0,
                arrow * 10.0,
                Stroke::new(1.6, MUTED),
            );
        }
        painter.text(
            arrow_at + Vec2::new(10.0, 0.0),
            Align2::LEFT_CENTER,
            format!(
                "{} {} · {}",
                format_distance(pin.distance_m),
                pin.direction,
                pin.size.label()
            ),
            FontId::proportional(11.5),
            MUTED,
        );
        let bar = Rect::from_min_max(
            Pos2::new(rect.right() - 118.0, rect.center().y - 4.0),
            Pos2::new(rect.right() - 52.0, rect.center().y + 4.0),
        );
        let fraction = ((pin.level_here_db - 30.0) / 80.0).clamp(0.0, 1.0);
        painter.rect_filled(bar, 4.0, Color32::from_rgb(40, 48, 54));
        painter.rect_filled(
            Rect::from_min_size(bar.min, Vec2::new(bar.width() * fraction, bar.height())),
            4.0,
            pin.color,
        );
        painter.text(
            rect.right_center() - Vec2::new(12.0, 0.0),
            Align2::RIGHT_CENTER,
            format!("{:.0} dB", pin.level_here_db),
            FontId::proportional(12.5),
            Color32::WHITE,
        );
        if play_response.clicked() {
            actions.push(WalkAction::Toggle {
                index: pin.index,
                on: pin.on,
            });
        } else if response.clicked() {
            actions.push(WalkAction::Select(pin.index));
        }
        if pin.selected {
            ui.add_space(2.0);
            if pin.size.adjustable {
                size_row(ui, pin, air_db_per_m, actions);
            }
            ui.label(
                egui::RichText::new(pin.size.caption())
                    .size(11.0)
                    .color(MUTED),
            );
        }
        ui.add_space(4.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn street_names_shorten_like_signs_without_touching_single_words() {
        assert_eq!(short_street_name("North Example Avenue"), "N Example Ave");
        assert_eq!(short_street_name("West Sample Street"), "W Sample St");
        assert_eq!(short_street_name("Broadway"), "Broadway");
        assert_eq!(short_street_name("East"), "East");
    }

    #[test]
    fn reach_grows_with_size_and_air_shortens_it() {
        let reaches = SIZE_PRESETS.map(|preset| reach_m(preset.spl_at_one_m_db, 0.005));
        assert!(reaches.windows(2).all(|pair| pair[0] < pair[1]));
        // Spherical spreading alone: 80 dB at 1 m meets 55 dB at ~17.8 m.
        assert!((reach_m(80.0, 0.0) - 17.78).abs() < 0.1);
        assert!(reach_m(135.0, 0.005) < reach_m(135.0, 0.0));
        assert_eq!(matching_preset(115.5), Some(2));
        assert_eq!(matching_preset(119.0), None);
    }

    #[test]
    fn heading_up_plan_puts_facing_up_and_round_trips() {
        let yaw = 30.0_f32.to_radians();
        let origin = [10.0, -4.0];
        for plan in [
            Plan::heading_up(origin, yaw, Pos2::new(200.0, 300.0), 2.0),
            Plan::fisheye(origin, yaw, Pos2::new(200.0, 300.0), 180.0, 40.0, 1500.0),
        ] {
            let ahead = [origin[0] + 50.0 * yaw.sin(), origin[1] + 50.0 * yaw.cos()];
            let screen = plan.project(ahead);
            assert!((screen.x - 200.0).abs() < 1.0e-3 && screen.y < 300.0);
            let back = plan.unproject(Pos2::new(260.0, 120.0));
            assert!(plan.project(back).distance(Pos2::new(260.0, 120.0)) < 1.0e-2);
        }
        let fisheye = Plan::fisheye(origin, 0.0, Pos2::ZERO, 180.0, 40.0, 1500.0);
        assert!((fisheye.radius_px(1500.0) - 180.0).abs() < 1.0e-3);
        assert!((fisheye.metres_at(fisheye.radius_px(320.0)) - 320.0).abs() < 0.05);
    }

    #[test]
    fn atlas_joins_named_ways_finds_corners_and_describes_place() {
        let names = [
            "West Alpha Avenue",
            "West Alpha Avenue",
            "North Beta Street",
            "",
        ]
        .map(String::from);
        let kinds = ["tertiary", "tertiary", "residential", "service"].map(String::from);
        let lines = vec![
            vec![[-100.0, 0.0], [0.0, 0.0]],
            vec![[0.0, 0.0], [100.0, 0.5]],
            vec![[50.0, -100.0], [50.0, 100.0]],
            vec![[20.0, -60.0], [20.0, 60.0]],
        ];
        let atlas = StreetAtlas::new(&lines, &names, &kinds);
        assert_eq!(atlas.names, ["W Alpha Ave", "N Beta St"]);
        assert_eq!(atlas.runs.len(), 2, "collinear same-name ways join");
        assert_eq!(atlas.corners.len(), 1);
        assert!(distance(atlas.corners[0].0, [50.0, 0.25]) < 0.5);
        let note = atlas.place_note([20.0, 30.0]);
        assert_eq!(note.here, "Alley");
        assert_eq!(note.near, "W Alpha Ave 30 m S · N Beta St 30 m E");
        assert_eq!(atlas.place_note([50.0, 40.0]).here, "N Beta St");
        assert_eq!(atlas.street_at([52.0, 40.0]), "N Beta St");
    }
}
