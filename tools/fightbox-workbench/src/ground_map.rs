//! Geometry-constrained illustrative ground influence. Never solver rays or measured energy.
use eframe::egui::{self, Color32, Pos2, Rect, Stroke};

pub(crate) type Point = [f32; 2];
pub(crate) type Wall = (Point, Point);

pub(crate) struct GroundMap {
    min: Point,
    spacing: f32,
    width: usize,
    height: usize,
    edges: Vec<[bool; 4]>,
    distances: Vec<f32>,
    key: Option<(usize, usize)>,
    pub bounds: (Point, Point),
    /// Display-only street centerlines; never used by acoustic calculations.
    pub street_lines: Vec<Vec<Point>>,
    pub walls: Vec<Wall>,
    pub roofs: Vec<[Point; 3]>,
}

impl GroundMap {
    pub(crate) fn new(
        bounds: (Point, Point),
        spacing: f32,
        street_lines: Vec<Vec<Point>>,
        walls: Vec<Wall>,
        roofs: Vec<[Point; 3]>,
    ) -> Self {
        // Bound setup and memory even for an unexpectedly large retained package.
        let spacing = spacing
            .max(0.5)
            .max((bounds.1[0] - bounds.0[0]).max(bounds.1[1] - bounds.0[1]) / 127.0);
        let width = ((bounds.1[0] - bounds.0[0]) / spacing).floor() as usize + 1;
        let height = ((bounds.1[1] - bounds.0[1]) / spacing).floor() as usize + 1;
        Self {
            min: bounds.0,
            spacing,
            width,
            height,
            edges: Vec::new(),
            distances: Vec::new(),
            key: None,
            bounds,
            street_lines,
            walls,
            roofs,
        }
    }

    fn position(&self, i: usize) -> Point {
        [
            self.min[0] + (i % self.width) as f32 * self.spacing,
            self.min[1] + (i / self.width) as f32 * self.spacing,
        ]
    }
    fn cell(&self, p: Point) -> usize {
        let x = ((p[0] - self.min[0]) / self.spacing)
            .round()
            .clamp(0.0, (self.width - 1) as f32) as usize;
        let y = ((p[1] - self.min[1]) / self.spacing)
            .round()
            .clamp(0.0, (self.height - 1) as f32) as usize;
        y * self.width + x
    }
    fn neighbours(&self, i: usize) -> [Option<usize>; 4] {
        let x = i % self.width;
        let y = i / self.width;
        [
            (x > 0).then(|| i - 1),
            (x + 1 < self.width).then(|| i + 1),
            (y > 0).then(|| i - self.width),
            (y + 1 < self.height).then(|| i + self.width),
        ]
    }
    /// Recompute only on selected-source or quantized source-cell changes.
    /// Wall-edge topology is built once, lazily when LIVE2D is first used.
    pub(crate) fn update(&mut self, selected: usize, source: Point) -> bool {
        let cell = self.cell(source);
        if self.key == Some((selected, cell)) {
            return false;
        }
        if self.edges.is_empty() {
            self.edges = (0..self.width * self.height)
                .map(|i| {
                    self.neighbours(i).map(|j| {
                        j.is_some_and(|j| {
                            !self.walls.iter().any(|&(a, b)| {
                                segments_touch(self.position(i), self.position(j), a, b)
                            })
                        })
                    })
                })
                .collect();
        }
        self.distances = vec![f32::INFINITY; self.width * self.height];
        // Nearest grid cell can be across a wall in a narrow street. Seed the
        // nearest locally visible cell instead; never tunnel through a wall.
        let seed = (0..self.distances.len())
            .filter(|&i| {
                let p = self.position(i);
                distance(p, source) <= self.spacing * 2.5
                    && !self
                        .walls
                        .iter()
                        .any(|&(a, b)| segments_touch(source, p, a, b))
            })
            .min_by(|&a, &b| {
                distance(self.position(a), source).total_cmp(&distance(self.position(b), source))
            });
        if let Some(seed) = seed {
            self.distances[seed] = distance(source, self.position(seed));
            let mut queue = std::collections::VecDeque::from([seed]);
            while let Some(i) = queue.pop_front() {
                for (direction, next) in self.neighbours(i).into_iter().enumerate() {
                    if let Some(j) = next {
                        if self.edges[i][direction] && !self.distances[j].is_finite() {
                            self.distances[j] = self.distances[i] + self.spacing;
                            queue.push_back(j);
                        }
                    }
                }
            }
        }
        self.key = Some((selected, cell));
        true
    }
    fn points(&self) -> impl Iterator<Item = (Point, f32)> + '_ {
        self.distances
            .iter()
            .enumerate()
            .filter_map(|(i, &d)| d.is_finite().then(|| (self.position(i), d)))
    }
}
fn distance(a: Point, b: Point) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}
fn orient(a: Point, b: Point, c: Point) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}
fn on_segment(a: Point, b: Point, p: Point) -> bool {
    orient(a, b, p).abs() < 1e-4
        && p[0] >= a[0].min(b[0]) - 1e-4
        && p[0] <= a[0].max(b[0]) + 1e-4
        && p[1] >= a[1].min(b[1]) - 1e-4
        && p[1] <= a[1].max(b[1]) + 1e-4
}
fn segments_touch(a: Point, b: Point, c: Point, d: Point) -> bool {
    let (u, v, w, z) = (
        orient(a, b, c),
        orient(a, b, d),
        orient(c, d, a),
        orient(c, d, b),
    );
    (u * v < 0.0 && w * z < 0.0)
        || on_segment(a, b, c)
        || on_segment(a, b, d)
        || on_segment(c, d, a)
        || on_segment(c, d, b)
}

/// Stable local framing. Source changes reset the frame; movement only reframes
/// at the inner edge, and never shrinks the frame continuously while walking.
#[derive(Default)]
pub(crate) struct LocalSoundFrame {
    cached: Option<(usize, bool, (Point, Point))>,
}
impl LocalSoundFrame {
    pub(crate) fn bounds(
        &mut self,
        selected: usize,
        source: Point,
        listener: Point,
    ) -> (Point, Point) {
        const MARGIN: f32 = 70.0;
        const EDGE: f32 = 20.0;
        let separation = distance(source, listener);
        let listener_local = match self.cached {
            Some((previous, true, _)) if previous == selected => separation > 140.0,
            _ => separation > 160.0,
        };
        if let Some((previous, was_local, bounds)) = self.cached {
            let inside = |point: Point| {
                (0..2).all(|axis| {
                    point[axis] > bounds.0[axis] + EDGE && point[axis] < bounds.1[axis] - EDGE
                })
            };
            if previous == selected
                && was_local == listener_local
                && (listener_local || inside(source))
                && inside(listener)
            {
                return bounds;
            }
        }
        let center = if listener_local {
            listener
        } else {
            [
                (source[0] + listener[0]) * 0.5,
                (source[1] + listener[1]) * 0.5,
            ]
        };
        let mut span = ((source[0] - listener[0])
            .abs()
            .max((source[1] - listener[1]).abs())
            + 2.0 * MARGIN)
            .max(160.0);
        if listener_local {
            span = 160.0;
        } else if let Some((previous, false, bounds)) = self.cached {
            if previous == selected {
                span = span.max(bounds.1[0] - bounds.0[0]);
            }
        }
        let half = span * 0.5;
        let bounds = (
            [center[0] - half, center[1] - half],
            [center[0] + half, center[1] + half],
        );
        self.cached = Some((selected, listener_local, bounds));
        bounds
    }
}

/// North-up metric projection, shared by geometry, field, sources and listener.
#[derive(Clone, Copy)]
pub(crate) struct MapProjection {
    center: Point,
    rect: Rect,
    scale: f32,
}
impl MapProjection {
    pub(crate) fn new(bounds: (Point, Point), rect: Rect) -> Self {
        Self {
            center: [
                (bounds.0[0] + bounds.1[0]) * 0.5,
                (bounds.0[1] + bounds.1[1]) * 0.5,
            ],
            rect,
            scale: (rect.width() / (bounds.1[0] - bounds.0[0]).max(1.0))
                .min(rect.height() / (bounds.1[1] - bounds.0[1]).max(1.0)),
        }
    }
    pub(crate) fn project(self, p: Point) -> Pos2 {
        Pos2::new(
            self.rect.center().x + (p[0] - self.center[0]) * self.scale,
            self.rect.center().y - (p[1] - self.center[1]) * self.scale,
        )
    }

    pub(crate) fn unproject(self, p: Pos2) -> Point {
        [
            self.center[0] + (p.x - self.rect.center().x) / self.scale,
            self.center[1] - (p.y - self.rect.center().y) / self.scale,
        ]
    }
}
/// Intersects a source-bearing ray with the marker-safe viewport edge.
/// The direction comes from actual source/listener positions, not a sound path.
pub(crate) fn source_edge_marker(listener: Pos2, source: Pos2, rect: Rect) -> Option<(Pos2, egui::Vec2)> {
    let direction = (source - listener).normalized();
    if !direction.is_finite() || direction == egui::Vec2::ZERO {
        return None;
    }
    let origin = rect.clamp(listener);
    let tx = if direction.x > 0.0 {
        (rect.right() - origin.x) / direction.x
    } else if direction.x < 0.0 {
        (rect.left() - origin.x) / direction.x
    } else {
        f32::INFINITY
    };
    let ty = if direction.y > 0.0 {
        (rect.bottom() - origin.y) / direction.y
    } else if direction.y < 0.0 {
        (rect.top() - origin.y) / direction.y
    } else {
        f32::INFINITY
    };
    Some((origin + direction * tx.min(ty), direction))
}

fn selected_marker_label(
    painter: &egui::Painter,
    safe: Rect,
    point: Pos2,
    text: String,
    color: Color32,
) {
    let galley = painter.layout(
        text,
        egui::FontId::monospace(11.0),
        color,
        safe.width().min(250.0),
    );
    let desired = point + egui::vec2(12.0, -galley.size().y - 8.0);
    let position = Pos2::new(
        desired.x.clamp(
            safe.left(),
            (safe.right() - galley.size().x).max(safe.left()),
        ),
        desired.y.clamp(
            safe.top(),
            (safe.bottom() - galley.size().y).max(safe.top()),
        ),
    );
    painter.rect_filled(
        Rect::from_min_size(position, galley.size()).expand(3.0),
        3.0,
        Color32::from_rgba_unmultiplied(12, 19, 24, 235),
    );
    painter.galley(position, galley, color);
}

pub(crate) struct Marker<'a> {
    pub position: Point,
    pub label: &'a str,
    pub selected: bool,
    pub enabled: bool,
}
pub(crate) struct LiveMapScene<'a> {
    pub map: &'a GroundMap,
    pub view_bounds: (Point, Point),
    pub markers: &'a [Marker<'a>],
    pub listener: Point,
    pub forward: Point,
    pub elevated: bool,
    pub active: bool,
    pub phase_s: f32,
    pub selected_label: &'a str,
    pub telemetry: &'a str,
    /// Developer diagnostics are opt-in. Clean listening mode keeps the
    /// geometry, listener/source markers, and source bearing only.
    pub diagnostic: bool,
}
/// Production painter also exercised by the device-free offscreen smoke.
pub(crate) fn paint(painter: &egui::Painter, rect: Rect, scene: LiveMapScene<'_>) {
    let painter = painter.with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, Color32::from_rgb(12, 19, 24));
    let map_rect = Rect::from_min_max(
        rect.min + egui::vec2(28.0, 80.0),
        rect.max - egui::vec2(28.0, 64.0),
    );
    if map_rect.width() < 1.0 || map_rect.height() < 1.0 {
        return;
    }
    let projection = MapProjection::new(scene.view_bounds, map_rect);
    let map_painter = painter.with_clip_rect(map_rect);
    for roof in &scene.map.roofs {
        map_painter.add(egui::Shape::convex_polygon(
            roof.iter().map(|&p| projection.project(p)).collect(),
            Color32::from_rgb(38, 49, 55),
            Stroke::NONE,
        ));
    }
    for line in &scene.map.street_lines {
        for segment in line.windows(2) {
            map_painter.line_segment(
                [projection.project(segment[0]), projection.project(segment[1])],
                Stroke::new(1.0, Color32::from_rgb(47, 68, 75)),
            );
        }
    }
    if scene.diagnostic && !scene.elevated && scene.phase_s >= 0.0 {
        for (point, d) in scene.map.points() {
            if d > 200.0 {
                continue;
            }
            let wave = ((d / 24.0 - scene.phase_s * 0.35) * std::f32::consts::TAU)
                .cos()
                .max(0.0)
                .powi(6);
            let alpha = ((18.0 + 100.0 * wave)
                * (1.0 - d / 220.0)
                * (if scene.active { 1.0 } else { 0.4 })) as u8;
            map_painter.circle_filled(
                projection.project(point),
                (scene.map.spacing * projection.scale * 0.36).clamp(1.0, 5.0),
                Color32::from_rgba_unmultiplied(223, 181, 94, alpha),
            );
        }
    }
    for &(a, b) in &scene.map.walls {
        map_painter.line_segment(
            [projection.project(a), projection.project(b)],
            Stroke::new(1.0, Color32::from_rgb(88, 105, 112)),
        );
    }
    // Reserve the measured-trace legend above and time strip below. Both are
    // overlaid by Workbench after this map painter returns.
    let marker_safe = Rect::from_min_max(
        map_rect.min + egui::vec2(12.0, 34.0),
        map_rect.max - egui::vec2(12.0, 56.0),
    );
    for marker in scene.markers {
        let point = projection.project(marker.position);
        let color = if marker.selected {
            Color32::from_rgb(255, 202, 113)
        } else {
            Color32::from_rgb(143, 155, 164)
        };
        if !map_rect.contains(point) && marker.selected && marker_safe.is_positive() {
            if let Some((edge, bearing)) =
                source_edge_marker(projection.project(scene.listener), point, marker_safe)
            {
                map_painter.arrow(
                    edge - bearing * 18.0,
                    bearing * 18.0,
                    Stroke::new(2.5, color),
                );
                selected_marker_label(
                    &map_painter,
                    marker_safe,
                    edge,
                    if scene.diagnostic {
                        format!(
                            "{}\nSource location · {:.0} m ground distance\nBearing, not sound-arrival direction",
                            marker.label,
                            distance(marker.position, scene.listener)
                        )
                    } else {
                        format!(
                            "{} · {:.0} m\nSource direction",
                            marker.label,
                            distance(marker.position, scene.listener)
                        )
                    },
                    color,
                );
            }
        } else {
            map_painter.circle_filled(point, if marker.enabled { 4.5 } else { 3.0 }, color);
            if !marker.selected && !scene.diagnostic && marker_safe.contains(point) {
                map_painter.text(
                    point + egui::vec2(8.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    marker.label,
                    egui::FontId::monospace(10.0),
                    color,
                );
            }
            if marker.selected && marker_safe.is_positive() {
                map_painter.circle_stroke(point, 8.0, Stroke::new(1.5, color));
                selected_marker_label(
                    &map_painter,
                    marker_safe,
                    point,
                    marker.label.to_owned(),
                    color,
                );
            }
        }
    }
    let listener = projection.project(scene.listener);
    map_painter.circle_filled(listener, 5.0, Color32::from_rgb(71, 220, 189));
    map_painter.arrow(
        listener,
        egui::vec2(scene.forward[0], -scene.forward[1]) * 18.0,
        Stroke::new(2.0, Color32::from_rgb(71, 220, 189)),
    );
    if !scene.diagnostic {
        map_painter.text(
            listener + egui::vec2(8.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            "You",
            egui::FontId::proportional(12.0),
            Color32::from_rgb(71, 220, 189),
        );
    }
    let text = |position, body: &str, size, color| {
        painter.text(
            position,
            egui::Align2::LEFT_TOP,
            body,
            egui::FontId::monospace(size),
            color,
        );
    };
    if scene.diagnostic {
        text(
            rect.min + egui::vec2(18.0, 14.0),
            &format!(
                "LIVE2D  /  {}{}",
                scene.selected_label,
                if scene.active { "" } else { " · source off" }
            ),
            15.0,
            Color32::from_rgb(225, 233, 232),
        );
        text(
            rect.min + egui::vec2(18.0, 40.0),
            scene.telemetry,
            11.0,
            Color32::from_rgb(161, 185, 190),
        );
        text(
            rect.min + egui::vec2(18.0, rect.height() - 46.0),
            if scene.elevated {
                "Elevated source: ground influence hidden; position remains shown."
            } else {
                "Illustrative around-wall influence · not solver rays or measured energy"
            },
            11.0,
            Color32::from_rgb(223, 181, 94),
        );
        text(
            rect.min + egui::vec2(18.0, rect.height() - 25.0),
            "North ↑  ·  WASD walk  ·  select a source above  ·  coarse ground grid",
            10.0,
            Color32::from_rgb(132, 159, 169),
        );
    } else {
        text(
            rect.min + egui::vec2(18.0, 18.0),
            "Your street",
            15.0,
            Color32::from_rgb(225, 233, 232),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_frame_keeps_corner_legible_and_stable_until_edge_or_selection_change() {
        let mut frame = LocalSoundFrame::default();
        let source = [102.5, 102.5];
        let initial = frame.bounds(0, source, [148.5, 106.0]);
        assert_eq!(initial.1[0] - initial.0[0], 186.0);
        assert_eq!(initial.1[1] - initial.0[1], 186.0);
        // Entire existing corner crossing fits without camera jitter.
        assert_eq!(frame.bounds(0, source, [148.5, 131.2]), initial);
        let recentered = frame.bounds(0, source, [220.0, 131.2]);
        assert_ne!(recentered, initial);
        for point in [source, [220.0, 131.2]] {
            for axis in 0..2 {
                assert!(point[axis] >= recentered.0[axis] + 69.9);
                assert!(point[axis] <= recentered.1[axis] - 69.9);
            }
        }
        // A distant source now preserves street-scale listener context;
        // its actual ground bearing is shown by an edge marker.
        let distant = frame.bounds(2, [482.5, 292.5], [148.5, 106.0]);
        assert_eq!(distant.1[0] - distant.0[0], 160.0);
        assert_eq!(frame.bounds(0, source, [148.5, 106.0]), initial);
        let minimum = frame.bounds(3, source, source);
        assert_eq!(minimum.1[0] - minimum.0[0], 160.0);
    }

    #[test]
    fn distant_source_keeps_four_meter_street_steps_legible_without_frame_jitter() {
        let mut frame = LocalSoundFrame::default();
        let source = [102.5, 102.5];
        let first = frame.bounds(0, source, [426.02, 483.82]);
        assert_eq!(first.1[0] - first.0[0], 160.0);
        for x in [430.02, 434.02, 438.02, 474.02] {
            assert_eq!(frame.bounds(0, source, [x, 483.82]), first);
        }
        assert_ne!(frame.bounds(0, source, [487.0, 483.82]), first);
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 400.0));
        let projection = MapProjection::new(first, screen);
        assert_eq!(
            (projection.project([438.02, 483.82]) - projection.project([434.02, 483.82])).length(),
            10.0
        );
        // Threshold hysteresis: small motion around160m cannot alternate zoom modes.
        let local = frame.bounds(1, [0.0, 0.0], [161.0, 0.0]);
        assert_eq!(frame.bounds(1, [0.0, 0.0], [159.0, 0.0]), local);
        assert!(
            frame.bounds(1, [0.0, 0.0], [139.0, 0.0]).1[0]
                - frame.bounds(1, [0.0, 0.0], [139.0, 0.0]).0[0]
                > 160.0
        );
    }

    #[test]
    fn offscreen_source_edge_marker_preserves_actual_bearing_and_clearance() {
        let rect = Rect::from_min_max(Pos2::new(12.0, 34.0), Pos2::new(588.0, 344.0));
        let listener = Pos2::new(300.0, 200.0);
        for source in [
            Pos2::new(-800.0, 900.0),
            Pos2::new(300.0, -800.0),
            Pos2::new(900.0, 200.0),
        ] {
            let (point, bearing) = source_edge_marker(listener, source, rect).unwrap();
            assert!(rect.expand(0.001).contains(point));
            assert!(
                (point.x - rect.left()).abs() < 0.001
                    || (point.x - rect.right()).abs() < 0.001
                    || (point.y - rect.top()).abs() < 0.001
                    || (point.y - rect.bottom()).abs() < 0.001
            );
            let delta = (point - listener).normalized();
            assert!(delta.dot((source - listener).normalized()) > 0.99999);
            assert!(delta.dot(bearing) > 0.99999);
        }
    }

    #[test]
    fn wall_on_grid_line_blocks_and_finite_wall_requires_detour() {
        let mut map = GroundMap::new(
            ([0.0, 0.0], [10.0, 10.0]),
            1.0,
            vec![],
            vec![([5.0, 0.0], [5.0, 7.0])],
            vec![],
        );
        map.update(0, [2.0, 5.0]);
        let d = map.points().find(|&(p, _)| p == [8.0, 5.0]).unwrap().1;
        assert!(
            d > 6.0,
            "must detour around wall endpoint, including grid-aligned contact"
        );
        let mut closed = GroundMap::new(
            ([0.0, 0.0], [10.0, 10.0]),
            1.0,
            vec![],
            vec![([5.0, 0.0], [5.0, 10.0])],
            vec![],
        );
        closed.update(0, [2.0, 5.0]);
        assert!(
            !closed.points().any(|(p, _)| p[0] > 5.0),
            "cannot leak through grid-aligned wall or boundary"
        );
    }
    #[test]
    fn source_selection_and_motion_invalidate_but_same_cell_reuses_field() {
        let mut map = GroundMap::new(([0.0, 0.0], [10.0, 10.0]), 1.0, vec![], vec![], vec![]);
        assert!(map.update(0, [1.0, 1.0]));
        assert!(!map.update(0, [1.1, 1.1]));
        assert!(map.update(1, [1.1, 1.1]));
        assert!(map.update(1, [8.0, 8.0]));
        assert_eq!(map.points().find(|&(p, _)| p == [8.0, 8.0]).unwrap().1, 0.0);
        assert!(map.points().find(|&(p, _)| p == [1.0, 1.0]).unwrap().1 > 10.0);
    }
    #[test]
    fn projection_is_north_up_uniform_and_centered() {
        let p = MapProjection::new(
            ([0.0, 0.0], [20.0, 10.0]),
            Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 400.0)),
        );
        assert_eq!(p.project([10.0, 5.0]), Pos2::new(200.0, 200.0));
        assert_eq!(p.project([0.0, 10.0]), Pos2::new(0.0, 100.0));
        assert_eq!(p.project([20.0, 0.0]), Pos2::new(400.0, 300.0));
    }
}

/// CPU-only egui raster evidence, used by the opted-in real-scene test. No
/// native window, GPU, audio device, screenshot permission or focus change.
#[cfg(test)]
pub(crate) mod offscreen {
    use super::*;
    use std::collections::HashMap;
    #[derive(Default)]
    pub(crate) struct Raster {
        textures: HashMap<egui::TextureId, egui::ColorImage>,
    }
    impl Raster {
        pub(crate) fn save(
            &mut self,
            ctx: &egui::Context,
            output: egui::FullOutput,
            path: &std::path::Path,
        ) -> usize {
            for (id, delta) in output.textures_delta.set {
                let egui::ImageData::Color(image) = delta.image;
                if let Some([x, y]) = delta.pos {
                    let base = self
                        .textures
                        .get_mut(&id)
                        .expect("partial texture follows full atlas");
                    for row in 0..image.size[1] {
                        for col in 0..image.size[0] {
                            base.pixels[(y + row) * base.size[0] + x + col] =
                                image.pixels[row * image.size[0] + col];
                        }
                    }
                } else {
                    self.textures.insert(id, (*image).clone());
                }
            }
            // Positions are in points; rasterize at the frame's pixel density.
            let ppp = output.pixels_per_point;
            let screen = ctx.viewport_rect();
            let size = [
                (screen.width() * ppp).round() as usize,
                (screen.height() * ppp).round() as usize,
            ];
            let mut rgb = vec![12u8; size[0] * size[1] * 3];
            let mut triangle_count = 0;
            for clipped in ctx.tessellate(output.shapes, output.pixels_per_point) {
                let egui::epaint::Primitive::Mesh(mesh) = clipped.primitive else {
                    continue;
                };
                let texture = self.textures.get(&mesh.texture_id);
                for tri in mesh.indices.chunks_exact(3) {
                    let v = [
                        mesh.vertices[tri[0] as usize],
                        mesh.vertices[tri[1] as usize],
                        mesh.vertices[tri[2] as usize],
                    ];
                    let p = v.map(|v| [v.pos.x * ppp, v.pos.y * ppp]);
                    let clip = clipped.clip_rect;
                    let area = orient(p[0], p[1], p[2]);
                    // A GPU discards non-finite triangles; NaN weights would
                    // otherwise pass every inside test and flood the clip.
                    if !area.is_finite() || area.abs() < 1e-8 {
                        continue;
                    }
                    triangle_count += 1;
                    let minx = p
                        .iter()
                        .map(|p| p[0])
                        .fold(f32::INFINITY, f32::min)
                        .max(clip.min.x * ppp)
                        .floor()
                        .max(0.0) as usize;
                    let maxx = p
                        .iter()
                        .map(|p| p[0])
                        .fold(f32::NEG_INFINITY, f32::max)
                        .min(clip.max.x * ppp)
                        .ceil()
                        .clamp(0.0, size[0] as f32) as usize;
                    let miny = p
                        .iter()
                        .map(|p| p[1])
                        .fold(f32::INFINITY, f32::min)
                        .max(clip.min.y * ppp)
                        .floor()
                        .max(0.0) as usize;
                    let maxy = p
                        .iter()
                        .map(|p| p[1])
                        .fold(f32::NEG_INFINITY, f32::max)
                        .min(clip.max.y * ppp)
                        .ceil()
                        .clamp(0.0, size[1] as f32) as usize;
                    for y in miny..maxy {
                        for x in minx..maxx {
                            let q = [x as f32 + 0.5, y as f32 + 0.5];
                            let weights = [
                                orient(p[1], p[2], q) / area,
                                orient(p[2], p[0], q) / area,
                                orient(p[0], p[1], q) / area,
                            ];
                            if weights.iter().any(|&w| w < 0.0) {
                                continue;
                            }
                            let tex = texture
                                .map(|t| {
                                    let u = (0..3).map(|i| weights[i] * v[i].uv.x).sum::<f32>();
                                    let w = (0..3).map(|i| weights[i] * v[i].uv.y).sum::<f32>();
                                    t.pixels[(w * t.size[1] as f32)
                                        .floor()
                                        .clamp(0.0, (t.size[1] - 1) as f32)
                                        as usize
                                        * t.size[0]
                                        + (u * t.size[0] as f32)
                                            .floor()
                                            .clamp(0.0, (t.size[0] - 1) as f32)
                                            as usize]
                                        .to_array()
                                })
                                .unwrap_or([255; 4]);
                            let color: [f32; 4] = std::array::from_fn(|channel| {
                                (0..3)
                                    .map(|i| {
                                        weights[i] * v[i].color.to_array()[channel] as f32 / 255.0
                                    })
                                    .sum::<f32>()
                                    * tex[channel] as f32
                                    / 255.0
                            });
                            let offset = (y * size[0] + x) * 3;
                            for channel in 0..3 {
                                rgb[offset + channel] = (color[channel] * 255.0
                                    + rgb[offset + channel] as f32 * (1.0 - color[3]))
                                    .clamp(0.0, 255.0)
                                    as u8;
                            }
                        }
                    }
                }
            }
            write_png(path, size, &rgb);
            triangle_count
        }
    }
    fn write_png(path: &std::path::Path, size: [usize; 2], rgb: &[u8]) {
        fn crc(bytes: &[u8]) -> u32 {
            let mut c = !0u32;
            for &b in bytes {
                c ^= b as u32;
                for _ in 0..8 {
                    c = (c >> 1) ^ if c & 1 != 0 { 0xedb88320 } else { 0 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let mut bytes = kind.to_vec();
            bytes.extend_from_slice(data);
            out.extend_from_slice(&crc(&bytes).to_be_bytes());
        }
        let mut raw = Vec::new();
        for row in rgb.chunks_exact(size[0] * 3) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        let mut zlib = vec![0x78, 0x01];
        let chunks = raw.chunks(65535);
        let count = chunks.len();
        for (i, part) in chunks.enumerate() {
            zlib.push(if i + 1 == count { 1 } else { 0 });
            let len = part.len() as u16;
            zlib.extend_from_slice(&len.to_le_bytes());
            zlib.extend_from_slice(&(!len).to_le_bytes());
            zlib.extend_from_slice(part);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &v in &raw {
            a = (a + v as u32) % 65521;
            b = (b + a) % 65521;
        }
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = (size[0] as u32).to_be_bytes().to_vec();
        header.extend_from_slice(&(size[1] as u32).to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &header);
        chunk(&mut png, b"IDAT", &zlib);
        chunk(&mut png, b"IEND", &[]);
        std::fs::write(path, png).unwrap();
    }
}
