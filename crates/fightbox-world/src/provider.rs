use fightbox_api::EnuVector3;
use serde_json::Value;

use crate::{Result, WorldError};

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderGeometry {
    pub vertices_enu_m: Vec<EnuVector3>,
    pub triangles: Vec<[u32; 3]>,
    pub material_names: Vec<String>,
    /// Interior rings are deliberately ignored in C1 and counted for provenance/reporting.
    pub ignored_hole_count: usize,
    /// Number of building features compiled by this provider.
    pub building_count: usize,
    /// Explicit disclosures for values supplied by compiler policy.
    pub assumptions: Vec<Assumption>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assumption {
    pub building_id: String,
    pub assumed_height_m: f32,
    pub reason: String,
}

pub trait TriangleProvider {
    fn provide(&self) -> Result<ProviderGeometry>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoJsonOptions {
    pub ground_margin_m: f32,
    pub default_building_material: String,
    pub ground_material: String,
    /// Height used only when neither `height` nor `building:levels`/`levels`
    /// is present. `None` preserves the strict C1 behavior.
    pub default_height_m: Option<f32>,
}

impl Default for GeoJsonOptions {
    fn default() -> Self {
        Self {
            ground_margin_m: 5.0,
            default_building_material: "brick".to_owned(),
            ground_material: "asphalt".to_owned(),
            default_height_m: None,
        }
    }
}

pub struct GeoJsonProvider<'a> {
    bytes: &'a [u8],
    options: GeoJsonOptions,
}

impl<'a> GeoJsonProvider<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8], options: GeoJsonOptions) -> Self {
        Self { bytes, options }
    }
}

impl TriangleProvider for GeoJsonProvider<'_> {
    fn provide(&self) -> Result<ProviderGeometry> {
        if !self.options.ground_margin_m.is_finite() || self.options.ground_margin_m < 0.0 {
            return Err(WorldError::InvalidGeoJson(
                "ground margin must be finite and non-negative".to_owned(),
            ));
        }
        if self
            .options
            .default_height_m
            .is_some_and(|height| !height.is_finite() || height <= 0.0)
        {
            return Err(WorldError::InvalidGeoJson(
                "default height must be finite and positive".to_owned(),
            ));
        }
        let root: Value = serde_json::from_slice(self.bytes)
            .map_err(|error| WorldError::Json(error.to_string()))?;
        if root.get("type").and_then(Value::as_str) != Some("FeatureCollection") {
            return Err(WorldError::InvalidGeoJson(
                "root type must be FeatureCollection".to_owned(),
            ));
        }
        let features = root
            .get("features")
            .and_then(Value::as_array)
            .ok_or_else(|| WorldError::InvalidGeoJson("features must be an array".to_owned()))?;
        if features.is_empty() {
            return Err(WorldError::InvalidGeoJson(
                "FeatureCollection contains no buildings".to_owned(),
            ));
        }

        let mut output = ProviderGeometry {
            vertices_enu_m: Vec::new(),
            triangles: Vec::new(),
            material_names: Vec::new(),
            ignored_hole_count: 0,
            building_count: features.len(),
            assumptions: Vec::new(),
        };
        let mut aabb = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for (feature_index, feature) in features.iter().enumerate() {
            let geometry = feature
                .get("geometry")
                .ok_or_else(|| invalid_feature(feature_index, "is missing geometry"))?;
            if geometry.get("type").and_then(Value::as_str) != Some("Polygon") {
                return Err(invalid_feature(
                    feature_index,
                    "geometry type must be Polygon",
                ));
            }
            let rings = geometry
                .get("coordinates")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_feature(feature_index, "coordinates must be an array"))?;
            let exterior = rings
                .first()
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_feature(feature_index, "has no exterior ring"))?;
            output.ignored_hole_count += rings.len().saturating_sub(1);
            let mut footprint = parse_ring(exterior, feature_index)?;
            normalize_ring(&mut footprint, feature_index)?;
            for point in &footprint {
                aabb[0] = aabb[0].min(point.0);
                aabb[1] = aabb[1].min(point.1);
                aabb[2] = aabb[2].max(point.0);
                aabb[3] = aabb[3].max(point.1);
            }

            let properties = feature.get("properties").and_then(Value::as_object);
            let (height, assumed) =
                height(properties, feature_index, self.options.default_height_m)?;
            if assumed {
                output.assumptions.push(Assumption {
                    building_id: building_id(feature, properties, feature_index),
                    assumed_height_m: height,
                    reason: "missing height and building:levels/levels; city default-height policy"
                        .to_owned(),
                });
            }
            let material = properties
                .and_then(|properties| properties.get("material"))
                .and_then(Value::as_str)
                .unwrap_or(&self.options.default_building_material)
                .to_owned();
            extrude(&footprint, height, &material, feature_index, &mut output)?;
        }
        add_ground(
            aabb,
            self.options.ground_margin_m,
            &self.options.ground_material,
            &mut output,
        )?;
        Ok(output)
    }
}

fn invalid_feature(feature: usize, message: &str) -> WorldError {
    WorldError::InvalidGeoJson(format!("feature {feature} {message}"))
}

fn height(
    properties: Option<&serde_json::Map<String, Value>>,
    feature: usize,
    default_height_m: Option<f32>,
) -> Result<(f32, bool)> {
    let raw = properties
        .and_then(|properties| properties.get("height"))
        .and_then(Value::as_f64)
        .or_else(|| {
            properties
                .and_then(|properties| {
                    properties
                        .get("building:levels")
                        .or_else(|| properties.get("levels"))
                })
                .and_then(Value::as_f64)
                .map(|levels| levels * 3.2)
        });
    let (raw, assumed) = raw.map_or_else(
        || {
            default_height_m.map_or_else(
                || {
                    Err(invalid_feature(
                        feature,
                        "requires numeric height or numeric building:levels/levels (levels use 3.2 m each)",
                    ))
                },
                |height| Ok((f64::from(height), true)),
            )
        },
        |height| Ok((height, false)),
    )?;
    if !raw.is_finite() || raw <= 0.0 || raw > f64::from(f32::MAX) {
        return Err(invalid_feature(
            feature,
            "height must be finite, positive, and representable as f32",
        ));
    }
    Ok((raw as f32, assumed))
}

fn building_id(
    feature: &Value,
    properties: Option<&serde_json::Map<String, Value>>,
    feature_index: usize,
) -> String {
    feature
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| {
            properties
                .and_then(|properties| properties.get("id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            properties
                .and_then(|properties| properties.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .unwrap_or_else(|| format!("feature/{feature_index}"))
}

fn parse_ring(points: &[Value], feature: usize) -> Result<Vec<(f64, f64)>> {
    let mut result = Vec::with_capacity(points.len());
    for (point_index, point) in points.iter().enumerate() {
        let coordinates = point.as_array().ok_or_else(|| {
            invalid_feature(
                feature,
                &format!("ring point {point_index} must be an array"),
            )
        })?;
        if coordinates.len() < 2 {
            return Err(invalid_feature(
                feature,
                &format!("ring point {point_index} needs east and north"),
            ));
        }
        let east = coordinates[0].as_f64().ok_or_else(|| {
            invalid_feature(
                feature,
                &format!("ring point {point_index} east must be numeric"),
            )
        })?;
        let north = coordinates[1].as_f64().ok_or_else(|| {
            invalid_feature(
                feature,
                &format!("ring point {point_index} north must be numeric"),
            )
        })?;
        if !east.is_finite()
            || !north.is_finite()
            || east.abs() > f64::from(f32::MAX)
            || north.abs() > f64::from(f32::MAX)
        {
            return Err(invalid_feature(
                feature,
                &format!("ring point {point_index} must be finite and representable as f32"),
            ));
        }
        result.push((east, north));
    }
    if result.len() >= 2 && result.first() == result.last() {
        result.pop();
    }
    if result.len() < 3 {
        return Err(invalid_feature(
            feature,
            "exterior ring needs at least three distinct points",
        ));
    }
    Ok(result)
}

fn normalize_ring(points: &mut [(f64, f64)], feature: usize) -> Result<()> {
    for index in 0..points.len() {
        let next = (index + 1) % points.len();
        if points[index] == points[next] {
            return Err(invalid_feature(
                feature,
                "exterior ring contains a zero-length edge",
            ));
        }
    }
    if self_intersects(points) {
        return Err(WorldError::SelfIntersectingPolygon { feature });
    }
    let area = signed_area(points);
    if area.abs() <= f64::EPSILON {
        return Err(invalid_feature(feature, "exterior ring has zero area"));
    }
    if area < 0.0 {
        points.reverse();
    }
    Ok(())
}

fn signed_area(points: &[(f64, f64)]) -> f64 {
    (0..points.len())
        .map(|index| {
            let next = (index + 1) % points.len();
            points[index].0 * points[next].1 - points[next].0 * points[index].1
        })
        .sum::<f64>()
        * 0.5
}

fn self_intersects(points: &[(f64, f64)]) -> bool {
    for first in 0..points.len() {
        let first_next = (first + 1) % points.len();
        for second in (first + 1)..points.len() {
            let second_next = (second + 1) % points.len();
            if first == second
                || first_next == second
                || second_next == first
                || (first == 0 && second_next == 0)
            {
                continue;
            }
            if segments_intersect(
                points[first],
                points[first_next],
                points[second],
                points[second_next],
            ) {
                return true;
            }
        }
    }
    false
}

fn segments_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let o1 = orientation(a, b, c);
    let o2 = orientation(a, b, d);
    let o3 = orientation(c, d, a);
    let o4 = orientation(c, d, b);
    if o1 == 0.0 && on_segment(a, b, c)
        || o2 == 0.0 && on_segment(a, b, d)
        || o3 == 0.0 && on_segment(c, d, a)
        || o4 == 0.0 && on_segment(c, d, b)
    {
        return true;
    }
    (o1 > 0.0) != (o2 > 0.0) && (o3 > 0.0) != (o4 > 0.0)
}

fn orientation(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

fn on_segment(a: (f64, f64), b: (f64, f64), point: (f64, f64)) -> bool {
    point.0 >= a.0.min(b.0)
        && point.0 <= a.0.max(b.0)
        && point.1 >= a.1.min(b.1)
        && point.1 <= a.1.max(b.1)
}

fn triangulate(points: &[(f64, f64)], feature: usize) -> Result<Vec<[usize; 3]>> {
    let mut remaining = (0..points.len()).collect::<Vec<_>>();
    let mut triangles = Vec::with_capacity(points.len().saturating_sub(2));
    while remaining.len() > 3 {
        let mut found = false;
        for cursor in 0..remaining.len() {
            let previous = remaining[(cursor + remaining.len() - 1) % remaining.len()];
            let current = remaining[cursor];
            let next = remaining[(cursor + 1) % remaining.len()];
            if orientation(points[previous], points[current], points[next]) <= 0.0 {
                continue;
            }
            if remaining.iter().copied().any(|candidate| {
                candidate != previous
                    && candidate != current
                    && candidate != next
                    && point_in_triangle(
                        points[candidate],
                        points[previous],
                        points[current],
                        points[next],
                    )
            }) {
                continue;
            }
            triangles.push([previous, current, next]);
            remaining.remove(cursor);
            found = true;
            break;
        }
        if !found {
            return Err(WorldError::TriangulationFailed { feature });
        }
    }
    triangles.push([remaining[0], remaining[1], remaining[2]]);
    Ok(triangles)
}

fn point_in_triangle(point: (f64, f64), a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> bool {
    const EPSILON: f64 = 1.0e-12;
    orientation(a, b, point) >= -EPSILON
        && orientation(b, c, point) >= -EPSILON
        && orientation(c, a, point) >= -EPSILON
}

fn extrude(
    points: &[(f64, f64)],
    height: f32,
    material: &str,
    feature: usize,
    output: &mut ProviderGeometry,
) -> Result<()> {
    let polygon_triangles = triangulate(points, feature)?;
    let base = u32::try_from(output.vertices_enu_m.len())
        .map_err(|_| invalid_feature(feature, "has too many vertices"))?;
    for &(east, north) in points {
        output
            .vertices_enu_m
            .push(EnuVector3::new(east as f32, north as f32, 0.0));
    }
    for &(east, north) in points {
        output
            .vertices_enu_m
            .push(EnuVector3::new(east as f32, north as f32, height));
    }
    let count = u32::try_from(points.len())
        .map_err(|_| invalid_feature(feature, "has too many ring points"))?;
    for [a, b, c] in polygon_triangles {
        let [a, b, c] = [a, b, c].map(|index| base + u32::try_from(index).expect("ring index"));
        push_triangle(output, [c, b, a], material);
        push_triangle(output, [a + count, b + count, c + count], material);
    }
    for index in 0..count {
        let next = (index + 1) % count;
        let bottom = base + index;
        let bottom_next = base + next;
        let top = bottom + count;
        let top_next = bottom_next + count;
        push_triangle(output, [bottom, bottom_next, top_next], material);
        push_triangle(output, [bottom, top_next, top], material);
    }
    Ok(())
}

fn add_ground(
    aabb: [f64; 4],
    margin: f32,
    material: &str,
    output: &mut ProviderGeometry,
) -> Result<()> {
    let margin = f64::from(margin);
    let coordinates = [
        (aabb[0] - margin, aabb[1] - margin),
        (aabb[2] + margin, aabb[1] - margin),
        (aabb[2] + margin, aabb[3] + margin),
        (aabb[0] - margin, aabb[3] + margin),
    ];
    if coordinates.iter().any(|&(east, north)| {
        !east.is_finite()
            || !north.is_finite()
            || east.abs() > f64::from(f32::MAX)
            || north.abs() > f64::from(f32::MAX)
    }) {
        return Err(WorldError::InvalidGeoJson(
            "ground AABB is not finite and representable as f32".to_owned(),
        ));
    }
    let base = u32::try_from(output.vertices_enu_m.len())
        .map_err(|_| WorldError::InvalidGeoJson("too many vertices".to_owned()))?;
    output.vertices_enu_m.extend(
        coordinates
            .into_iter()
            .map(|(east, north)| EnuVector3::new(east as f32, north as f32, 0.0)),
    );
    push_triangle(output, [base, base + 1, base + 2], material);
    push_triangle(output, [base, base + 2, base + 3], material);
    Ok(())
}

fn push_triangle(output: &mut ProviderGeometry, triangle: [u32; 3], material: &str) {
    output.triangles.push(triangle);
    output.material_names.push(material.to_owned());
}

pub struct ObjProvider<'a> {
    bytes: &'a [u8],
    material: String,
}

impl<'a> ObjProvider<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8], material: impl Into<String>) -> Self {
        Self {
            bytes,
            material: material.into(),
        }
    }
}

impl TriangleProvider for ObjProvider<'_> {
    fn provide(&self) -> Result<ProviderGeometry> {
        let text = std::str::from_utf8(self.bytes)
            .map_err(|error| WorldError::InvalidObj(format!("input is not UTF-8: {error}")))?;
        let mut vertices = Vec::new();
        let mut triangles = Vec::new();
        let mut material_names = Vec::new();
        let mut active_material = self.material.clone();
        for (line_index, line) in text.lines().enumerate() {
            let line_number = line_index + 1;
            let meaningful = line.split('#').next().unwrap_or("").trim();
            if meaningful.is_empty() {
                continue;
            }
            let fields = meaningful.split_whitespace().collect::<Vec<_>>();
            match fields[0] {
                "v" => {
                    if fields.len() != 4 {
                        return Err(WorldError::InvalidObj(format!(
                            "line {line_number}: v requires exactly three coordinates"
                        )));
                    }
                    let coordinates = fields[1..]
                        .iter()
                        .map(|value| {
                            value.parse::<f32>().map_err(|_| {
                                WorldError::InvalidObj(format!(
                                    "line {line_number}: invalid vertex coordinate {value:?}"
                                ))
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    vertices.push(EnuVector3::new(
                        coordinates[0],
                        coordinates[1],
                        coordinates[2],
                    ));
                }
                "f" => {
                    if fields.len() != 4 {
                        return Err(WorldError::InvalidObj(format!(
                            "line {line_number}: only triangulated faces are supported; found {} vertices",
                            fields.len().saturating_sub(1)
                        )));
                    }
                    let mut triangle = [0_u32; 3];
                    for (destination, field) in triangle.iter_mut().zip(&fields[1..]) {
                        let vertex_field = field.split('/').next().unwrap_or("");
                        let raw = vertex_field.parse::<i64>().map_err(|_| {
                            WorldError::InvalidObj(format!(
                                "line {line_number}: invalid face index {field:?}"
                            ))
                        })?;
                        if raw <= 0 {
                            return Err(WorldError::InvalidObj(format!(
                                "line {line_number}: OBJ indices must be positive in C1"
                            )));
                        }
                        *destination = u32::try_from(raw - 1).map_err(|_| {
                            WorldError::InvalidObj(format!(
                                "line {line_number}: face index is too large"
                            ))
                        })?;
                    }
                    triangles.push(triangle);
                    material_names.push(active_material.clone());
                }
                "usemtl" => {
                    if fields.len() != 2 || fields[1].trim().is_empty() {
                        return Err(WorldError::InvalidObj(format!(
                            "line {line_number}: usemtl requires exactly one material name"
                        )));
                    }
                    active_material = fields[1].to_owned();
                }
                _ => {}
            }
        }
        if vertices.is_empty() || triangles.is_empty() {
            return Err(WorldError::InvalidObj(
                "input must contain v statements and triangular f statements".to_owned(),
            ));
        }
        Ok(ProviderGeometry {
            vertices_enu_m: vertices,
            triangles,
            material_names,
            ignored_hole_count: 0,
            building_count: 0,
            assumptions: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{GeoJsonOptions, GeoJsonProvider, TriangleProvider, triangulate};
    use crate::WorldError;

    #[test]
    fn ear_clips_a_concave_polygon() {
        let polygon = vec![(0.0, 0.0), (3.0, 0.0), (3.0, 3.0), (1.5, 1.0), (0.0, 3.0)];
        assert_eq!(triangulate(&polygon, 0).unwrap().len(), 3);
    }

    #[test]
    fn counts_ignored_holes() {
        let json = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"height":4},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]],[[0.5,0.5],[1,0.5],[1,1],[0.5,0.5]]]}}]}"#;
        let geometry = GeoJsonProvider::new(json, GeoJsonOptions::default())
            .provide()
            .unwrap();
        assert_eq!(geometry.ignored_hole_count, 1);
    }

    #[test]
    fn rejects_self_intersection() {
        let json = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"height":4},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,2],[0,2],[2,0],[0,0]]]}}]}"#;
        assert!(matches!(
            GeoJsonProvider::new(json, GeoJsonOptions::default()).provide(),
            Err(WorldError::SelfIntersectingPolygon { feature: 0 })
        ));
    }
}
