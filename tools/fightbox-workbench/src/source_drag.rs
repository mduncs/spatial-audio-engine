use std::time::Instant;

use eframe::egui::{Pos2, Vec2};
use fightbox_api::EnuVector3;

use crate::acoustic_state::{ProbeCoverage, ProbeCoverageQuery};
use crate::ground_map::{MapProjection, Point};

pub(crate) struct SourceDrag {
    pub index: usize,
    pub bounds: (Point, Point),
    projection: MapProjection,
    offset: Vec2,
    pub height_above_ground_m: f32,
    pub candidate: EnuVector3,
    pub last_covered: EnuVector3,
    pub covered: bool,
    pub planned: EnuVector3,
    pub last_plan: Instant,
}

impl SourceDrag {
    pub fn new(
        index: usize,
        bounds: (Point, Point),
        projection: MapProjection,
        pointer: Pos2,
        position: EnuVector3,
        height_above_ground_m: f32,
    ) -> Self {
        Self {
            index,
            bounds,
            projection,
            offset: projection.project([position.east_m, position.north_m]) - pointer,
            height_above_ground_m,
            candidate: position,
            last_covered: position,
            covered: true,
            planned: position,
            last_plan: Instant::now(),
        }
    }

    pub fn update(
        &mut self,
        pointer: Pos2,
        coverage: &ProbeCoverageQuery,
        ground_height: impl Fn(Point) -> f32,
    ) -> EnuVector3 {
        let [east, north] = self.projection.unproject(pointer + self.offset);
        self.candidate = EnuVector3::new(
            east,
            north,
            ground_height([east, north]) + self.height_above_ground_m,
        );
        self.covered = coverage.coverage(self.candidate) == ProbeCoverage::Covered;
        if self.covered {
            self.last_covered = self.candidate;
        }
        self.last_covered
    }

    pub fn pointer_for_last_covered(&self) -> Pos2 {
        self.projection
            .project([self.last_covered.east_m, self.last_covered.north_m])
            - self.offset
    }
}

/// Top surface at this XY, using the package's outward-wound ground and roofs.
pub(crate) fn ground_height(mesh: &fightbox_world::AcousticMesh, point: Point) -> f32 {
    mesh.triangles
        .iter()
        .filter_map(|triangle| {
            let [a, b, c] = triangle.map(|index| mesh.vertices_enu_m[index as usize]);
            let denominator = (b.north_m - c.north_m) * (a.east_m - c.east_m)
                + (c.east_m - b.east_m) * (a.north_m - c.north_m);
            if denominator <= 1.0e-6 {
                return None;
            }
            let u = ((b.north_m - c.north_m) * (point[0] - c.east_m)
                + (c.east_m - b.east_m) * (point[1] - c.north_m))
                / denominator;
            let v = ((c.north_m - a.north_m) * (point[0] - c.east_m)
                + (a.east_m - c.east_m) * (point[1] - c.north_m))
                / denominator;
            let w = 1.0 - u - v;
            (u >= -1.0e-5 && v >= -1.0e-5 && w >= -1.0e-5)
                .then_some(u * a.up_m + v * b.up_m + w * c.up_m)
        })
        .max_by(f32::total_cmp)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{self, Rect};

    #[test]
    fn map_drag_round_trip_keeps_grab_offset_and_height() {
        let bounds = ([-100.0, 30.0], [300.0, 230.0]);
        let projection = MapProjection::new(
            bounds,
            Rect::from_min_size(Pos2::new(50.0, 75.0), egui::vec2(800.0, 500.0)),
        );
        let position = EnuVector3::new(25.0, 90.0, 11.1);
        let pointer = projection.project([25.0, 90.0]) + egui::vec2(3.0, -4.0);
        let mut drag = SourceDrag::new(0, bounds, projection, pointer, position, 1.5);
        let coverage = ProbeCoverageQuery::from_fn(|_| true);
        assert_eq!(drag.update(pointer, &coverage, |_| 9.6), position);
        let target = projection.project([75.0, 140.0]) + egui::vec2(3.0, -4.0);
        assert_eq!(
            drag.update(target, &coverage, |_| 0.0),
            EnuVector3::new(75.0, 140.0, 1.5)
        );
        assert_eq!(
            projection.unproject(projection.project([-100.0, 230.0])),
            [-100.0, 230.0]
        );
        let mesh = fightbox_world::AcousticMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, 0.0, 0.0),
                EnuVector3::new(20.0, 0.0, 10.0),
                EnuVector3::new(0.0, 20.0, 0.0),
            ],
            triangles: vec![[0, 1, 2], [2, 1, 0]],
            material_ids: vec![0, 0],
        };
        assert_eq!(ground_height(&mesh, [10.0, 5.0]), 5.0);
        assert_eq!(ground_height(&mesh, [30.0, 5.0]), 0.0);
        let raised = EnuVector3::new(25.0, 90.0, 35.0);
        let mut drag = SourceDrag::new(0, bounds, projection, pointer, raised, 1.5);
        assert_eq!(drag.update(pointer, &coverage, |_| 9.6).up_m, 11.1);
    }

    #[test]
    fn uncovered_drop_snaps_to_last_covered_not_drag_origin() {
        let bounds = ([0.0, 0.0], [100.0, 100.0]);
        let projection = MapProjection::new(
            bounds,
            Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 400.0)),
        );
        let position = EnuVector3::new(10.0, 20.0, 1.5);
        let mut drag = SourceDrag::new(
            0,
            bounds,
            projection,
            projection.project([10.0, 20.0]),
            position,
            1.5,
        );
        let coverage = ProbeCoverageQuery::from_fn(|p| p.east_m <= 50.0 && p.up_m == 1.5);
        assert_eq!(
            drag.update(projection.project([40.0, 30.0]), &coverage, |_| 0.0),
            EnuVector3::new(40.0, 30.0, 1.5)
        );
        assert!(drag.covered);
        assert_eq!(
            drag.update(projection.project([80.0, 35.0]), &coverage, |_| 0.0),
            EnuVector3::new(40.0, 30.0, 1.5)
        );
        assert!(!drag.covered);
        assert_eq!(drag.candidate, EnuVector3::new(80.0, 35.0, 1.5));
        assert_eq!(
            drag.update(
                projection.project([20.0, 20.0]),
                &ProbeCoverageQuery::unavailable(),
                |_| 0.0
            ),
            drag.last_covered
        );
        assert!(!drag.covered, "unknown coverage cannot authorize a drop");
    }
}
