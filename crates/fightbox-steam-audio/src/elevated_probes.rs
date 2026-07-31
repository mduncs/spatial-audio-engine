//! Mid-air probe layers merged into the same batch as the floor probes.
//!
//! Steam Audio 4.8.1 only ships `IPL_PROBEGENERATIONTYPE_UNIFORMFLOOR`, which
//! ray-casts downward and drops probes a fixed height above whatever solid
//! surface it finds. Its own header says the algorithm "is not suitable for
//! scenarios where the listener may fly into a region with no probes", and that
//! is exactly the elevated case: a source hovering over a street canyon has no
//! influencing probe and therefore no baked pathing at all.
//!
//! Raising `height` on a second uniform-floor volume does not solve it either.
//! The generated layer follows the terrain *and every rooftop*, so a city block
//! produces a bumpy shell rather than a flat layer, and the probe count roughly
//! doubles for probes that mostly sit far above the geometry that matters.
//!
//! This module instead places the elevated probes itself and hands them to
//! `iplProbeBatchAddProbe`, which merges them into the same `IPLProbeBatch` the
//! floor array went into. One batch is baked and one batch is loaded, so nothing
//! downstream changes.

use crate::{EnuVector3, ProbeVolume, SceneMesh};

/// Hard ceiling on samples per axis, so a nonsensical spacing cannot allocate
/// its way out of the process before validation reports the real problem.
const MAX_AXIS_STEPS: usize = 4_096;

/// One horizontal layer of manually placed probes at a fixed ENU altitude.
///
/// The layer spans the horizontal extent of the bake's [`ProbeVolume`]. Each
/// probe's influence radius is its spacing, matching the radius Steam Audio's
/// uniform-floor generator assigns (verified against 4.8.1: radius is exactly
/// 4.0 m at 4 m spacing and exactly 8.0 m at 8 m spacing).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ElevatedProbeLayer {
    /// Absolute ENU up coordinate of every probe in the layer, in metres.
    pub height_enu_m: f32,
    /// Horizontal spacing between neighbouring probes, in metres. Also the
    /// influence radius, so a layer always covers its own cells.
    pub spacing_m: f32,
}

/// Probe centres for `layer`, in ENU metres.
///
/// The grid matches the one `IPL_PROBEGENERATIONTYPE_UNIFORMFLOOR` lays down for
/// the same volume and spacing: `floor(span / spacing) + 1` samples per axis,
/// centred in the span. Probes that fall inside solid geometry are dropped —
/// they cannot see out of the building they are buried in, and their influence
/// spheres would otherwise reach into the open air beside the facade and shadow
/// the usable probes there.
pub(crate) fn layer_probe_centers(
    volume: ProbeVolume,
    layer: ElevatedProbeLayer,
    mesh: &SceneMesh,
) -> Vec<EnuVector3> {
    let east = axis_samples(volume.min_enu_m.x, volume.max_enu_m.x, layer.spacing_m);
    let north = axis_samples(volume.min_enu_m.y, volume.max_enu_m.y, layer.spacing_m);
    let mut centers = Vec::with_capacity(east.len() * north.len());
    for x in &east {
        for y in &north {
            let center = EnuVector3::new(*x, *y, layer.height_enu_m);
            if !is_inside_solid(mesh, center) {
                centers.push(center);
            }
        }
    }
    centers
}

/// Sample positions along one axis, centred in `[min, max]`.
fn axis_samples(min: f32, max: f32, spacing: f32) -> Vec<f32> {
    let span = max - min;
    if !span.is_finite() || span < 0.0 || !spacing.is_finite() || spacing <= 0.0 {
        return Vec::new();
    }
    // A layer denser than this is a mistake, not a request; the bake would run
    // for years. Validation rejects it before generation is ever reached.
    let steps = (span / spacing).floor().clamp(0.0, MAX_AXIS_STEPS as f32) as usize;
    let start = min + (span - steps as f32 * spacing) * 0.5;
    (0..=steps)
        .map(|index| start + index as f32 * spacing)
        .collect()
}

/// Whether `point` lies inside closed geometry, by counting how many triangles a
/// straight-up ray crosses. An odd count means the ray started inside.
///
/// This assumes the solids above `point` are closed, which holds for the
/// extruded building boxes the world compiler emits. A ground quad below the
/// point is never crossed by an upward ray, so open terrain does not perturb the
/// count.
fn is_inside_solid(mesh: &SceneMesh, point: EnuVector3) -> bool {
    let mut crossings: Vec<f64> = Vec::new();
    for triangle in &mesh.triangles {
        let Some(vertices) = triangle_vertices(mesh, *triangle) else {
            continue;
        };
        if let Some(distance) = upward_ray_hit(point, vertices) {
            crossings.push(distance);
        }
    }
    // A ray grazing a shared edge is reported once per adjacent triangle. Fusing
    // near-identical hits keeps the parity meaningful.
    crossings.sort_by(f64::total_cmp);
    crossings.dedup_by(|left, right| (*left - *right).abs() <= 1.0e-6);
    crossings.len() % 2 == 1
}

fn triangle_vertices(mesh: &SceneMesh, triangle: [i32; 3]) -> Option<[EnuVector3; 3]> {
    let mut vertices = [EnuVector3::new(0.0, 0.0, 0.0); 3];
    for (slot, index) in vertices.iter_mut().zip(triangle) {
        *slot = *mesh.vertices_enu_m.get(usize::try_from(index).ok()?)?;
    }
    Some(vertices)
}

/// Möller–Trumbore specialised to the `+Z` ray direction, in `f64`.
fn upward_ray_hit(origin: EnuVector3, triangle: [EnuVector3; 3]) -> Option<f64> {
    let edge1 = subtract(triangle[1], triangle[0]);
    let edge2 = subtract(triangle[2], triangle[0]);
    // cross(direction = (0, 0, 1), edge2)
    let p = [-edge2[1], edge2[0], 0.0];
    let determinant = dot(edge1, p);
    if determinant.abs() <= 1.0e-12 {
        return None;
    }
    let inverse = 1.0 / determinant;
    let translated = subtract(origin, triangle[0]);
    let u = dot(translated, p) * inverse;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(translated, edge1);
    let v = q[2] * inverse;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let distance = dot(edge2, q) * inverse;
    (distance > 1.0e-6).then_some(distance)
}

fn subtract(left: EnuVector3, right: EnuVector3) -> [f64; 3] {
    [
        f64::from(left.x) - f64::from(right.x),
        f64::from(left.y) - f64::from(right.y),
        f64::from(left.z) - f64::from(right.z),
    ]
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AcousticMaterial;

    /// A closed axis-aligned box, twelve triangles, outward winding.
    fn box_mesh(min: EnuVector3, max: EnuVector3) -> SceneMesh {
        let vertices_enu_m = vec![
            EnuVector3::new(min.x, min.y, min.z),
            EnuVector3::new(max.x, min.y, min.z),
            EnuVector3::new(max.x, max.y, min.z),
            EnuVector3::new(min.x, max.y, min.z),
            EnuVector3::new(min.x, min.y, max.z),
            EnuVector3::new(max.x, min.y, max.z),
            EnuVector3::new(max.x, max.y, max.z),
            EnuVector3::new(min.x, max.y, max.z),
        ];
        let triangles = vec![
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
        ];
        SceneMesh {
            material_indices: vec![0; triangles.len()],
            triangles,
            vertices_enu_m,
            materials: vec![AcousticMaterial::MASONRY],
        }
    }

    #[test]
    fn axis_samples_match_the_uniform_floor_grid() {
        // The megablock bake's own extent: 71 columns of 8 m centred in a
        // 560.5 m span, which is where Steam Audio 4.8.1 put its floor probes.
        let samples = axis_samples(12.3, 572.8, 8.0);
        assert_eq!(samples.len(), 71);
        assert!((samples[0] - 12.55).abs() < 1.0e-3);
        assert!((samples[70] - 572.55).abs() < 1.0e-3);
    }

    #[test]
    fn axis_samples_survive_a_span_shorter_than_the_spacing() {
        let samples = axis_samples(0.0, 3.0, 8.0);
        assert_eq!(samples.len(), 1);
        assert!((samples[0] - 1.5).abs() < 1.0e-6);
        assert!(axis_samples(0.0, 10.0, 0.0).is_empty());
    }

    #[test]
    fn probes_inside_a_building_are_dropped_and_open_air_probes_are_kept() {
        let mesh = box_mesh(
            EnuVector3::new(-5.0, -5.0, 0.0),
            EnuVector3::new(5.0, 5.0, 60.0),
        );
        assert!(is_inside_solid(&mesh, EnuVector3::new(0.0, 0.0, 30.0)));
        // Beside the box, and directly above its roof, are both open air.
        assert!(!is_inside_solid(&mesh, EnuVector3::new(20.0, 0.0, 30.0)));
        assert!(!is_inside_solid(&mesh, EnuVector3::new(0.0, 0.0, 70.0)));
    }

    #[test]
    fn layer_covers_the_volume_footprint_minus_the_building() {
        let mesh = box_mesh(
            EnuVector3::new(-5.0, -5.0, 0.0),
            EnuVector3::new(5.0, 5.0, 60.0),
        );
        let volume = ProbeVolume {
            min_enu_m: EnuVector3::new(-20.0, -20.0, 0.0),
            max_enu_m: EnuVector3::new(20.0, 20.0, 63.0),
            spacing_m: 10.0,
            height_above_floor_m: 3.0,
        };
        let layer = ElevatedProbeLayer {
            height_enu_m: 30.0,
            spacing_m: 10.0,
        };
        let centers = layer_probe_centers(volume, layer, &mesh);
        // A 5x5 grid (-20, -10, 0, 10, 20 on each axis) with the single column
        // through the box removed.
        assert_eq!(centers.len(), 5 * 5 - 1);
        assert!(centers.iter().all(|center| center.z == 30.0));
        assert!(!centers.contains(&EnuVector3::new(0.0, 0.0, 30.0)));
        assert!(centers.contains(&EnuVector3::new(10.0, 0.0, 30.0)));
    }
}
