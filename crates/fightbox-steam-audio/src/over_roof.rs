//! A vertical-plane rooftop approximation for sources without baked transport.
//!
//! Single-edge screening is Astra's signed ITU knife-edge J(nu), including
//! the illuminated side. Multiple edges use the ISO 9613-2:2024 rubber-band
//! geometry and C3 (eqs. 18--22), with Kmet=1 and its 25 dB multi-edge cap.
//! This is not coupled rigid-wedge diffraction or a shortest 3-D route.
//! Geometry and dominant-path selection run only on the simulation thread.

use crate::backend_snapshot::SteamDirectParams;
use crate::route_voicing::{air_gain, realizable_eq};
use crate::{SceneMesh, SteamVector3};
use fightbox_runtime::THREE_BAND_AIR_REFERENCE_HZ;

const C: f64 = 343.0;
const REBUILD_METERS: f32 = 0.05;

/// Fixed direct-epoch payload; true emitter pose and reflection sends stay intact.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RoofTransport {
    pub active: bool,
    pub arrival_position: SteamVector3,
    pub extra_distance_m: f32,
    pub distance_attenuation: f32,
    pub band_gains: [f32; 3],
}

impl RoofTransport {
    pub fn direct(self, original: SteamDirectParams) -> SteamDirectParams {
        if !self.active {
            return original;
        }
        SteamDirectParams {
            distance_attenuation: self.distance_attenuation,
            air_absorption: self.band_gains,
            directivity: original.directivity,
            // J is the TOTAL barrier field. Steam visibility is not applied twice.
            occlusion: 1.0,
            transmission: [1.0; 3],
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RoofTriangle {
    points: [[f64; 2]; 3],
    height: f64,
    min: [f64; 2],
    max: [f64; 2],
}

/// Exact footprint slices from the package's horizontal upward roof triangles.
/// Ground and prism bottoms are excluded. No display-map or GeoJSON dependency.
pub(crate) struct RoofProfile {
    roofs: Vec<RoofTriangle>,
}

impl RoofProfile {
    pub fn from_mesh(mesh: &SceneMesh) -> Self {
        let roofs = mesh
            .triangles
            .iter()
            .filter_map(|tri| {
                let [a, b, c] = tri.map(|i| mesh.vertices_enu_m[i as usize]);
                if a.z <= 0.05 || (a.z - b.z).abs() > 0.001 || (a.z - c.z).abs() > 0.001 {
                    return None;
                }
                let normal = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
                if normal <= 0.0 {
                    return None;
                }
                let points = [a, b, c].map(|v| [f64::from(v.x), f64::from(v.y)]);
                Some(RoofTriangle {
                    min: std::array::from_fn(|axis| {
                        points.iter().map(|p| p[axis]).fold(f64::INFINITY, f64::min)
                    }),
                    max: std::array::from_fn(|axis| {
                        points
                            .iter()
                            .map(|p| p[axis])
                            .fold(f64::NEG_INFINITY, f64::max)
                    }),
                    points,
                    height: f64::from(a.z),
                })
            })
            .collect();
        Self { roofs }
    }

    pub fn payload_bytes(&self) -> u64 {
        (self.roofs.capacity() * std::mem::size_of::<RoofTriangle>()) as u64
    }

    fn solve(&self, source: SteamVector3, listener: SteamVector3) -> Option<RoofPath> {
        let s = [f64::from(source.x), -f64::from(source.z)];
        let l = [f64::from(listener.x), -f64::from(listener.z)];
        let range = (l[0] - s[0]).hypot(l[1] - s[1]);
        if range < 0.001 {
            return None;
        }
        let u = [(l[0] - s[0]) / range, (l[1] - s[1]) / range];
        let mut candidates = Vec::new();
        for roof in &self.roofs {
            // A listener/emitter below a roof inside its footprint is indoors,
            // where the exterior roof route is not a valid arrival. This also
            // preserves clear interior routes in non-city meshes with ceilings.
            let within = |p: [f64; 2]| {
                let side = |a: [f64; 2], b: [f64; 2]| {
                    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
                };
                (0..3).all(|i| side(roof.points[i], roof.points[(i + 1) % 3]) >= -1e-8)
            };
            if (roof.height > f64::from(source.y) + 0.001 && within(s))
                || (roof.height > f64::from(listener.y) + 0.001 && within(l))
            {
                return None;
            }
            if (0..2).any(|axis| {
                roof.min[axis] > s[axis].max(l[axis]) || roof.max[axis] < s[axis].min(l[axis])
            }) {
                continue;
            }
            for i in 0..3 {
                let a = roof.points[i];
                let b = roof.points[(i + 1) % 3];
                let side = |p: [f64; 2]| u[0] * (p[1] - s[1]) - u[1] * (p[0] - s[0]);
                let sa = side(a);
                let sb = side(b);
                if sa * sb > 0.0 {
                    continue;
                }
                if (sa - sb).abs() < 1e-10 {
                    // A ray along a triangle edge must retain BOTH endpoints.
                    for p in [a, b] {
                        let d = (p[0] - s[0]) * u[0] + (p[1] - s[1]) * u[1];
                        if d > 1e-5 && d < range - 1e-5 {
                            candidates.push([d, roof.height]);
                        }
                    }
                } else {
                    let t = sa / (sa - sb);
                    let d = (a[0] + t * (b[0] - a[0]) - s[0]) * u[0]
                        + (a[1] + t * (b[1] - a[1]) - s[1]) * u[1];
                    if d > 1e-5 && d < range - 1e-5 {
                        candidates.push([d, roof.height]);
                    }
                }
            }
        }
        solve_profile(
            range,
            f64::from(source.y),
            f64::from(listener.y),
            candidates,
        )
        .map(|mut path| {
            if path.edge_count > 0 {
                path.arrival_position = SteamVector3::new(
                    (s[0] + u[0] * path.last_edge[0]) as f32,
                    path.last_edge[1] as f32,
                    -(s[1] + u[1] * path.last_edge[0]) as f32,
                );
            } else {
                path.arrival_position = source;
            }
            path
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct RoofPath {
    straight_m: f64,
    length_m: f64,
    delta_m: f64,
    edge_span_m: f64,
    edge_count: usize,
    last_edge: [f64; 2],
    /// Signed excess through the retained edge on the illuminated side.
    signed_excess_m: f64,
    arrival_position: SteamVector3,
}

fn segment(a: [f64; 2], b: [f64; 2]) -> f64 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

fn solve_profile(
    range: f64,
    source_z: f64,
    listener_z: f64,
    mut candidates: Vec<[f64; 2]>,
) -> Option<RoofPath> {
    if candidates.is_empty() {
        return None;
    }
    let s = [0.0, source_z];
    let l = [range, listener_z];
    let straight = segment(s, l);
    // Retain the most significant edge even after it leaves the shadow hull.
    let mut signed_excess = f64::NEG_INFINITY;
    for &p in &candidates {
        let excess = (segment(s, p) + segment(p, l) - straight).max(0.0);
        let clearance = p[1] - (source_z + (listener_z - source_z) * p[0] / range);
        let signed = if clearance >= 0.0 { excess } else { -excess };
        signed_excess = signed_excess.max(signed);
    }
    candidates.sort_unstable_by(|a, b| a[0].total_cmp(&b[0]).then(b[1].total_cmp(&a[1])));
    candidates.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-6);
    let mut hull = vec![s];
    for p in candidates.into_iter().chain(std::iter::once(l)) {
        while hull.len() >= 2 {
            let a = hull[hull.len() - 2];
            let b = hull[hull.len() - 1];
            let cross = (b[0] - a[0]) * (p[1] - b[1]) - (b[1] - a[1]) * (p[0] - b[0]);
            if cross < -1e-8 {
                break;
            }
            hull.pop();
        }
        hull.push(p);
    }
    let edge_count = hull.len() - 2;
    let length = hull.windows(2).map(|p| segment(p[0], p[1])).sum::<f64>();
    // e is the SUM along the hull, not the chord between its end edges.
    let edge_span = if edge_count > 1 {
        hull[1..hull.len() - 1]
            .windows(2)
            .map(|p| segment(p[0], p[1]))
            .sum()
    } else {
        0.0
    };
    Some(RoofPath {
        straight_m: straight,
        length_m: length,
        delta_m: (length - straight).max(0.0),
        edge_span_m: edge_span,
        edge_count,
        last_edge: hull[hull.len() - 2],
        signed_excess_m: signed_excess,
        arrival_position: SteamVector3::default(),
    })
}

fn knife_edge_db(nu: f64) -> f64 {
    let t = nu - 0.1;
    // asinh avoids cancellation on the deeply illuminated side.
    (6.9 + 20.0 * t.asinh() / std::f64::consts::LN_10).max(0.0)
}

impl RoofPath {
    fn screening_db(self, hz: f64) -> f64 {
        let lambda = C / hz;
        if self.edge_count > 1 {
            let q = (5.0 * lambda / self.edge_span_m).powi(2);
            let c3 = (1.0 + q) / (1.0 / 3.0 + q);
            let zmin = -2.0 * lambda / (20.0 * c3);
            // ISO 2024 eq.18 with Kmet=1; no ground filter is added.
            (10.0 * (1.0 + 20.0 * c3 * (self.delta_m - zmin) / lambda).log10()).clamp(0.0, 25.0)
        } else {
            let excess = if self.edge_count == 1 {
                self.delta_m
            } else {
                self.signed_excess_m
            };
            knife_edge_db(excess.signum() * (4.0 * excess.abs() / lambda).sqrt())
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RoofCache {
    endpoints: Option<(SteamVector3, SteamVector3)>,
    path: Option<RoofPath>,
    transmission: [f32; 3],
}

impl RoofCache {
    pub fn update(
        &mut self,
        profile: &RoofProfile,
        source: SteamVector3,
        listener: SteamVector3,
        direct: SteamDirectParams,
        air_exponents: [f32; 3],
        baked_owns_transport: bool,
    ) -> RoofTransport {
        if baked_owns_transport {
            *self = Self::default();
            return RoofTransport::default();
        }
        let moved = |a: SteamVector3, b: SteamVector3| {
            (a.x - b.x).hypot(a.y - b.y).hypot(a.z - b.z) > REBUILD_METERS
        };
        if self
            .endpoints
            .is_none_or(|(s, l)| moved(s, source) || moved(l, listener))
        {
            self.path = profile.solve(source, listener);
            self.endpoints = Some((source, listener));
        }
        let Some(path) = self.path else {
            self.transmission = [0.0; 3];
            return RoofTransport::default();
        };
        if path.edge_count > 0 && direct.occlusion >= 1.0 - 1e-6 {
            return RoofTransport::default();
        }
        if direct.occlusion < 0.5 {
            self.transmission = direct.transmission;
        }
        let screen = THREE_BAND_AIR_REFERENCE_HZ
            .map(|f| 10.0_f64.powf(-path.screening_db(f64::from(f)) / 20.0));
        let diff_air = air_gain(path.length_m, air_exponents);
        let direct_air = air_gain(path.straight_m, air_exponents);
        let diff =
            std::array::from_fn::<_, 3, _>(|b| screen[b] * diff_air[b] / path.length_m.max(1.0));
        let trans = std::array::from_fn::<_, 3, _>(|b| {
            f64::from(self.transmission[b]) * direct_air[b] / path.straight_m.max(1.0)
        });
        let energy = |g: [f64; 3]| g.into_iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-20);
        let advantage_db = 20.0 * (energy(diff) / energy(trans)).log10();
        let t = ((advantage_db + 3.0) / 6.0).clamp(0.0, 1.0);
        let weight = t * t * (3.0 - 2.0 * t);
        // One pressure transfer, one read head; no two correlated PCM taps.
        let length = path.straight_m + weight * path.delta_m;
        let distance = 1.0 / length.max(1.0);
        let gains = realizable_eq(std::array::from_fn(|b| {
            (weight * diff[b] + (1.0 - weight) * trans[b]) / distance
        }));
        let arrival = path.arrival_position;
        RoofTransport {
            active: true,
            extra_distance_m: (length - path.straight_m).min(f64::from(
                crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS,
            )) as f32,
            distance_attenuation: distance as f32,
            band_gains: gains,
            arrival_position: SteamVector3::new(
                (f64::from(source.x) * (1.0 - weight) + f64::from(arrival.x) * weight) as f32,
                (f64::from(source.y) * (1.0 - weight) + f64::from(arrival.y) * weight) as f32,
                (f64::from(source.z) * (1.0 - weight) + f64::from(arrival.z) * weight) as f32,
            ),
        }
    }
}

/// One continuously moving direct read head on the existing PCM history.
/// Reflections continue reading the original straight-line head. Topology and
/// transmission handovers move this head; they never crossfade duplicate PCM.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RoofReadHead {
    initialized: bool,
    extra_samples: f32,
    history_samples: usize,
    retention: f32,
}

impl RoofReadHead {
    pub fn invalidate(&mut self) {
        *self = Self::default();
    }
    pub fn next_extra_samples(&mut self, transport: RoofTransport, sample_rate: i32) -> f32 {
        let target = if transport.active {
            transport.extra_distance_m * sample_rate as f32 / 343.0
        } else {
            0.0
        };
        if !self.initialized {
            self.extra_samples = target;
            self.initialized = true;
            self.retention = (-1.0 / (0.080 * sample_rate as f32)).exp();
        }
        let step = (target - self.extra_samples) * (1.0 - self.retention);
        self.extra_samples += step.clamp(-0.5, 0.5);
        if self.extra_samples.abs() < 1e-5 {
            self.extra_samples = 0.0;
        }
        self.history_samples = self.history_samples.saturating_add(1);
        self.extra_samples
    }
    pub fn read_mono(
        &mut self,
        line: &crate::propagation_delay::PropagationDelayLine,
        transport: RoofTransport,
        straight_output: f32,
        sample_rate: i32,
    ) -> f32 {
        let extra = self.next_extra_samples(transport, sample_rate);
        if extra == 0.0 {
            return straight_output;
        }
        line.read_behind_newest(line.current_delay_samples() + extra, self.history_samples)
    }

    pub fn history_samples(&self) -> usize {
        self.history_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn astra_wall_geometry_and_screening() {
        let path = solve_profile(104.0, 30.0, 1.5, vec![[100.0, 12.0]]).unwrap();
        assert!((path.straight_m - 107.8344).abs() < 0.0001);
        assert!((path.length_m - 112.8432).abs() < 0.0001);
        assert!((path.delta_m - 5.0088).abs() < 0.0001);
        assert!((1000.0 * path.length_m / C - 329.0).abs() < 0.1);
        for (hz, db) in [(125.0, 21.53), (1000.0, 30.51), (4000.0, 36.56)] {
            assert!((path.screening_db(hz) - db).abs() < 1.5);
        }
    }
    #[test]
    fn astra_shadow_boundary_walk_is_continuous_and_monotone() {
        for (x, expected) in [
            (68.333333, [3.91, 1.01, 0.0]),
            (58.333333, [6.03; 3]),
            (48.333333, [8.76, 12.52, 18.28]),
        ] {
            let p = solve_profile(100.0 + x, 30.0, 1.5, vec![[100.0, 12.0]]).unwrap();
            for (hz, db) in THREE_BAND_AIR_REFERENCE_HZ.into_iter().zip(expected) {
                assert!(
                    (p.screening_db(f64::from(hz)) - db).abs() < 1.0,
                    "x={x}, hz={hz}"
                );
            }
        }
        let mut previous = [0.0; 3];
        for i in 0..2001 {
            let x = 68.333333 - f64::from(i) * 0.01;
            let p = solve_profile(100.0 + x, 30.0, 1.5, vec![[100.0, 12.0]]).unwrap();
            let db = THREE_BAND_AIR_REFERENCE_HZ.map(|hz| p.screening_db(f64::from(hz)));
            for b in 0..3 {
                assert!(db[b] + 1e-6 >= previous[b]);
                if i > 0 {
                    assert!(db[b] - previous[b] < 1.0);
                }
            }
            previous = db;
        }
    }
    #[test]
    fn multiple_edges_use_hull_arc_and_iso_2024_rule() {
        let p = solve_profile(
            100.0,
            1.5,
            1.5,
            vec![[20.0, 25.0], [50.0, 35.0], [80.0, 25.0]],
        )
        .unwrap();
        assert_eq!(p.edge_count, 3);
        assert!((p.edge_span_m - 2.0 * 30.0_f64.hypot(10.0)).abs() < 1e-8);
        let lambda = C / 400.0;
        let q = (5.0 * lambda / p.edge_span_m).powi(2);
        let expected = (10.0
            * (3.0 + 20.0 * ((1.0 + q) / (1.0 / 3.0 + q)) * p.delta_m / lambda).log10())
        .min(25.0);
        assert!((p.screening_db(400.0) - expected).abs() < 1e-8);
    }
    fn roof_mesh() -> SceneMesh {
        SceneMesh {
            vertices_enu_m: vec![
                crate::EnuVector3::new(-1.0, -5.0, 12.0),
                crate::EnuVector3::new(0.0, -5.0, 12.0),
                crate::EnuVector3::new(0.0, 5.0, 12.0),
                crate::EnuVector3::new(-1.0, 5.0, 12.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            material_indices: vec![0, 0],
            materials: vec![crate::AcousticMaterial::MASONRY],
        }
    }

    #[test]
    fn package_roof_axes_ownership_and_single_pcm_arrival() {
        let profile = RoofProfile::from_mesh(&roof_mesh());
        let s = SteamVector3::new(-100.0, 30.0, 0.0);
        let l = SteamVector3::new(4.0, 1.5, 0.0);
        let path = profile.solve(s, l).unwrap();
        assert!((path.length_m - 112.8432).abs() < 0.0001);
        assert_eq!(path.arrival_position, SteamVector3::new(0.0, 12.0, 0.0));
        let raw = SteamDirectParams {
            distance_attenuation: 1.0 / path.straight_m as f32,
            air_absorption: [1.0; 3],
            directivity: 1.0,
            occlusion: 0.0,
            transmission: [0.001; 3],
        };
        let mut cache = RoofCache::default();
        assert!(!cache.update(&profile, s, l, raw, [0.0; 3], true).active);
        let transport = cache.update(&profile, s, l, raw, [0.0; 3], false);
        assert!(transport.active);
        assert!((transport.extra_distance_m - float_delta(path)).abs() < 0.0001);
        assert_eq!(transport.direct(raw).occlusion, 1.0);
        let sr = 48_000;
        let mut line = crate::propagation_delay::PropagationDelayLine::new(30_000, sr);
        line.reset_to((path.straight_m / C * sr as f64) as f32);
        let mut head = RoofReadHead::default();
        let mut output = Vec::new();
        for frame in 0..20_000 {
            let straight = line.process_sample(if frame == 0 { 1.0 } else { 0.0 });
            output.push(head.read_mono(&line, transport, straight, sr));
        }
        let peak = output
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        assert!((peak as f64 - path.length_m / C * sr as f64).abs() < 2.0);
        let straight_frame = (path.straight_m / C * sr as f64) as usize;
        assert!(
            output[straight_frame - 3..straight_frame + 4]
                .iter()
                .all(|x| *x == 0.0),
            "no duplicate direct arrival"
        );
        assert!(output.iter().all(|x| x.is_finite()));
    }
    fn float_delta(path: RoofPath) -> f32 {
        path.delta_m as f32
    }

    #[test]
    fn clear_direct_and_interior_ceilings_do_not_trigger_a_shadow_route() {
        let profile = RoofProfile::from_mesh(&roof_mesh());
        let s = SteamVector3::new(-100.0, 30.0, 0.0);
        let l = SteamVector3::new(4.0, 1.5, 0.0);
        let clear = SteamDirectParams {
            distance_attenuation: 0.01,
            air_absorption: [1.0; 3],
            directivity: 1.0,
            occlusion: 1.0,
            transmission: [1.0; 3],
        };
        assert!(
            !RoofCache::default()
                .update(&profile, s, l, clear, [0.0; 3], false)
                .active
        );
        assert!(
            profile
                .solve(s, SteamVector3::new(-0.5, 1.5, 0.0))
                .is_none()
        );
        assert!(
            profile
                .solve(SteamVector3::new(-0.5, 1.5, 0.0), l)
                .is_none()
        );
    }

    #[test]
    fn rooftop_head_reads_stereo_coherently_and_rejects_retired_history() {
        let mut mono = crate::propagation_delay::PropagationDelayLine::new(2000, 48_000);
        let mut stereo = crate::propagation_delay::StereoProgramPropagationDelay::new(2000, 48_000);
        mono.reset_to(101.25);
        stereo.reset_to(101.25);
        for frame in 0..1500 {
            let input = (frame as f32 * 0.019).sin();
            let _ = mono.process_sample(input);
            stereo.process_frame([input, -input], 2).unwrap();
            let a = mono.read_behind_newest(137.875, frame + 1);
            let b = stereo.read_behind_newest(137.875, frame + 1);
            assert!((a - b[0]).abs() < 1e-6);
            assert!((b[0] + b[1]).abs() < 1e-7);
        }
        stereo.invalidate();
        stereo.process_frame([0.0; 2], 2).unwrap();
        assert_eq!(stereo.read_behind_newest(137.875, 1500), [0.0; 2]);
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    #[ignore = "requires OVERROOF_EVIDENCE_DIR and OVERROOF_SCENE_PACKAGE; writes off-repo evidence"]
    fn loop_city_walk_evidence() {
        use crate::backend_snapshot::SteamSourcePropagation;
        use crate::motion_smoothing::SourcePropagationSmoother;
        use fightbox_api::{EnuVector3 as V, ListenerState, Pose};
        use fightbox_runtime::backend::{MAX_ACTIVE_SOURCES, SimulationUpdate, SourceMotion};
        use std::io::Write;
        let package = std::path::PathBuf::from(std::env::var_os("OVERROOF_SCENE_PACKAGE").unwrap());
        let output = std::path::PathBuf::from(std::env::var_os("OVERROOF_EVIDENCE_DIR").unwrap());
        assert!(output.is_absolute() && !output.starts_with(std::env::current_dir().unwrap()));
        let bytes = std::fs::read(package.join("mesh.bin")).unwrap();
        assert_eq!(&bytes[..8], b"FBXMESH\0");
        let u32_at = |o| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let nv = u32_at(12) as usize;
        let nt = u32_at(16) as usize;
        let material_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(package.join("materials.json")).unwrap())
                .unwrap();
        let materials = material_json
            .as_object()
            .unwrap()
            .values()
            .map(|v| crate::AcousticMaterial {
                absorption: std::array::from_fn(|i| v["absorption"][i].as_f64().unwrap() as f32),
                scattering: v["scattering"].as_f64().unwrap() as f32,
                transmission: std::array::from_fn(|i| {
                    v["transmission"][i].as_f64().unwrap() as f32
                }),
            })
            .collect();
        let mesh = SceneMesh {
            vertices_enu_m: (0..nv)
                .map(|i| {
                    let f = |j: usize| {
                        f32::from_le_bytes(
                            bytes[20 + i * 12 + j * 4..24 + i * 12 + j * 4]
                                .try_into()
                                .unwrap(),
                        )
                    };
                    crate::EnuVector3::new(f(0), f(1), f(2))
                })
                .collect(),
            triangles: (0..nt)
                .map(|i| std::array::from_fn(|j| u32_at(20 + nv * 12 + i * 12 + j * 4) as i32))
                .collect(),
            material_indices: (0..nt)
                .map(|i| u32_at(20 + nv * 12 + nt * 12 + i * 4) as i32)
                .collect(),
            materials,
        };
        let profile = RoofProfile::from_mesh(&mesh);
        let static_fixture = std::env::var_os("OVERROOF_STATIC_FIXTURE");
        let fixture: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                output.join(
                    static_fixture
                        .as_deref()
                        .unwrap_or(std::ffi::OsStr::new("walk.json")),
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let source: [f32; 3] = std::array::from_fn(|i| {
            fixture["sources"][0]["position_m"][i]
                .as_f64()
                .or_else(|| fixture["sources"][0]["trajectory"]["waypoints_m"][0][i].as_f64())
                .unwrap() as f32
        });
        let src = V::new(source[0], source[1], source[2]);
        let pose = |p| Pose {
            position: p,
            forward: V::new(0.0, 1.0, 0.0),
            up: V::new(0.0, 0.0, 1.0),
        };
        let cfg = crate::S3SimulationConfig::default();
        let (mut sim, _graph) = crate::linked::build_roof_evidence_generation(
            &mesh,
            None,
            crate::AudioConfig {
                sample_rate_hz: 48_000,
                frame_size: 128,
            },
            cfg,
            &[crate::MultiSourceDescriptor::at(src)],
            1,
            crate::QualityTier::Desktop,
        )
        .unwrap();
        if static_fixture.is_some() {
            let p = &fixture["listener"]["position_m"];
            let listener = V::new(
                p[0].as_f64().unwrap() as f32,
                p[1].as_f64().unwrap() as f32,
                p[2].as_f64().unwrap() as f32,
            );
            let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
            sources[0] = SourceMotion {
                active: true,
                pose: pose(src),
                linear_velocity_mps: V::default(),
            };
            sim.update_inputs(&SimulationUpdate {
                listener: ListenerState {
                    pose: pose(listener),
                    linear_velocity_mps: V::default(),
                },
                sources,
            });
            sim.run_direct().unwrap();
            let snap = sim.roof_evidence_snapshot();
            let p = snap.sources[0];
            println!(
                "static roof: direct={:?}; roof={:?}; geometry={:?}",
                p.direct,
                p.over_roof,
                profile.solve(p.source_position, snap.listener_position)
            );
            assert!(p.over_roof.active);
            return;
        }
        let mut csv = std::fs::File::create(output.join("walk-screening.csv")).unwrap();
        writeln!(csv,"time_s,north_m,screen_400_db,screen_2530_db,screen_13266_db,smoothed_400_db,smoothed_2530_db,smoothed_13266_db,arrival_x_m,arrival_north_m,arrival_up_m,extra_m,occlusion").unwrap();
        let mut smoother = SourcePropagationSmoother::default();
        let mut previous_db = [0.0; 3];
        let mut previous_dir: Option<[f64; 3]> = None;
        let mut worst_step = 0.0_f64;
        let mut worst_angle = 0.0_f64;
        let mut cache = RoofCache::default();
        let mut normalized = [1.0_f64; 3];
        let retention = (-1.0_f64 / (60.0 * 0.080)).exp();
        for i in 0..=1200 {
            let time = i as f64 / 60.0;
            let start: [f64; 3] = std::array::from_fn(|j| {
                fixture["listener"]["trajectory"]["waypoints_m"][0][j]
                    .as_f64()
                    .unwrap()
            });
            let end: [f64; 3] = std::array::from_fn(|j| {
                fixture["listener"]["trajectory"]["waypoints_m"][1][j]
                    .as_f64()
                    .unwrap()
            });
            let p: [f32; 3] =
                std::array::from_fn(|j| (start[j] + (end[j] - start[j]) * time / 20.0) as f32);
            let listener = V::new(p[0], p[1], p[2]);
            let x = f64::from(p[1]);
            let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
            sources[0] = SourceMotion {
                active: true,
                pose: pose(src),
                linear_velocity_mps: V::default(),
            };
            sim.update_inputs(&SimulationUpdate {
                listener: ListenerState {
                    pose: pose(listener),
                    linear_velocity_mps: V::new(0.0, -0.9, 0.0),
                },
                sources,
            });
            sim.run_direct().unwrap();
            let snap = sim.roof_evidence_snapshot();
            let published = snap.sources[0];
            assert!(published.over_roof.active);
            let actual = profile
                .solve(published.source_position, snap.listener_position)
                .unwrap();
            let db = THREE_BAND_AIR_REFERENCE_HZ.map(|hz| actual.screening_db(hz as f64));
            for b in 0..3 {
                if i > 0 {
                    assert!(db[b] + 1e-4 >= previous_db[b]);
                    worst_step = worst_step.max((db[b] - previous_db[b]).abs());
                }
                previous_db[b] = db[b];
            }
            // Normalize before EQ floors, air and spreading. Retain the same
            // geometry cache and 80 ms gain slew as the product route.
            let _ = cache.update(
                &profile,
                published.source_position,
                snap.listener_position,
                published.direct,
                [0.0; 3],
                false,
            );
            let cached = cache.path.unwrap();
            let gain = THREE_BAND_AIR_REFERENCE_HZ
                .map(|hz| 10.0_f64.powf(-cached.screening_db(hz as f64) / 20.0));
            for b in 0..3 {
                normalized[b] = gain[b] + (normalized[b] - gain[b]) * retention;
            }
            let smooth_db = normalized.map(|g| -20.0 * g.log10());
            let smoothed = smoother
                .advance(
                    SteamSourcePropagation {
                        over_roof: published.over_roof,
                        ..published
                    },
                    snap.listener_position,
                    0.9,
                    retention as f32,
                )
                .endpoint();
            let arr = smoothed.arrival_position;
            let dv = [
                (arr.x - listener.east_m) as f64,
                (-arr.z - listener.north_m) as f64,
                (arr.y - listener.up_m) as f64,
            ];
            let norm = dv.iter().map(|v| v * v).sum::<f64>().sqrt();
            let dir = dv.map(|v| v / norm);
            if let Some(prev) = previous_dir {
                let dot = dir
                    .into_iter()
                    .zip(prev)
                    .map(|(a, b)| a * b)
                    .sum::<f64>()
                    .clamp(-1.0, 1.0);
                worst_angle = worst_angle.max(dot.acos().to_degrees());
            }
            previous_dir = Some(dir);
            writeln!(csv,"{time:.6},{x:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",db[0],db[1],db[2],smooth_db[0],smooth_db[1],smooth_db[2],arr.x,-arr.z,arr.y,published.over_roof.extra_distance_m,published.direct.occlusion).unwrap();
        }
        assert!(worst_step < 1.0, "normalized screening step {worst_step}");
        assert!(worst_angle < 5.0, "direction step {worst_angle}");
        println!(
            "real city walk: worst normalized screening step {worst_step:.6} dB at 60 Hz; worst arrival turn {worst_angle:.6} degrees; roof cache {} bytes",
            profile.payload_bytes()
        );
    }
}
