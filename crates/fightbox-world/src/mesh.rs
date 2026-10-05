use fightbox_api::EnuVector3;

use crate::{MaterialTable, Result, TriangleProvider, WorldError};

/// Indexed acoustic geometry in right-handed local ENU metres.
///
/// Provider-generated solids use outward winding: roofs and the ground face `+Z`,
/// prism bottoms face `-Z`, and walls face away from the footprint interior.
#[derive(Clone, Debug, PartialEq)]
pub struct AcousticMesh {
    pub vertices_enu_m: Vec<EnuVector3>,
    pub triangles: Vec<[u32; 3]>,
    pub material_ids: Vec<u32>,
}

impl AcousticMesh {
    pub fn validate(&self, material_count: usize, triangle_budget: usize) -> Result<()> {
        if self.triangles.len() > triangle_budget {
            return Err(WorldError::TriangleBudgetExceeded {
                actual: self.triangles.len(),
                budget: triangle_budget,
            });
        }
        for (index, vertex) in self.vertices_enu_m.iter().enumerate() {
            if !vertex.is_finite() {
                return Err(WorldError::NonFiniteVertex { vertex: index });
            }
        }
        if self.material_ids.len() != self.triangles.len() {
            return Err(WorldError::MissingMaterialAssignment {
                triangle: self.material_ids.len().min(self.triangles.len()),
            });
        }
        for (triangle_index, triangle) in self.triangles.iter().enumerate() {
            let mut vertices = [EnuVector3::default(); 3];
            for (destination, source) in vertices.iter_mut().zip(triangle) {
                let index = usize::try_from(*source).map_err(|_| WorldError::IndexOutOfRange {
                    triangle: triangle_index,
                    index: *source,
                })?;
                *destination =
                    *self
                        .vertices_enu_m
                        .get(index)
                        .ok_or(WorldError::IndexOutOfRange {
                            triangle: triangle_index,
                            index: *source,
                        })?;
            }
            let ab = subtract(vertices[1], vertices[0]);
            let ac = subtract(vertices[2], vertices[0]);
            let normal = cross(ab, ac);
            let area_squared = f64::from(normal.east_m).powi(2)
                + f64::from(normal.north_m).powi(2)
                + f64::from(normal.up_m).powi(2);
            if area_squared == 0.0 {
                return Err(WorldError::DegenerateTriangle {
                    triangle: triangle_index,
                });
            }
            let material_id = self.material_ids[triangle_index];
            if usize::try_from(material_id).map_or(true, |id| id >= material_count) {
                return Err(WorldError::UnknownMaterial(format!("#{material_id}")));
            }
        }
        Ok(())
    }
}

/// Serializes an acoustic mesh as deterministic, triangulated Wavefront OBJ.
///
/// Material table names are emitted with `usemtl`; the paired [`crate::ObjProvider`]
/// consumes those statements, so exporting and importing preserves each face's
/// material assignment as well as its indexed triangle and `f32` vertex values.
pub fn export_obj(mesh: &AcousticMesh, materials: &MaterialTable) -> Result<Vec<u8>> {
    materials.validate()?;
    mesh.validate(materials.iter().len(), usize::MAX)?;
    let names = materials.iter().map(|(name, _)| name).collect::<Vec<_>>();
    let mut output = String::from(
        "# fightbox acoustic mesh\n# coordinates: local ENU metres (x=east, y=north, z=up)\n",
    );
    for vertex in &mesh.vertices_enu_m {
        output.push_str(&format!(
            "v {} {} {}\n",
            vertex.east_m, vertex.north_m, vertex.up_m
        ));
    }
    let mut active_material = None;
    for (triangle, material_id) in mesh.triangles.iter().zip(&mesh.material_ids) {
        if active_material != Some(*material_id) {
            let name = names
                .get(*material_id as usize)
                .ok_or_else(|| WorldError::UnknownMaterial(format!("#{material_id}")))?;
            output.push_str("usemtl ");
            output.push_str(name);
            output.push('\n');
            active_material = Some(*material_id);
        }
        output.push_str(&format!(
            "f {} {} {}\n",
            triangle[0] + 1,
            triangle[1] + 1,
            triangle[2] + 1
        ));
    }
    Ok(output.into_bytes())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileOptions {
    pub triangle_budget: usize,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            triangle_budget: 1_000_000,
        }
    }
}

pub fn compile(
    provider: &impl TriangleProvider,
    materials: &MaterialTable,
    options: CompileOptions,
) -> Result<AcousticMesh> {
    materials.validate()?;
    let geometry = provider.provide()?;
    if geometry.triangles.len() > options.triangle_budget {
        return Err(WorldError::TriangleBudgetExceeded {
            actual: geometry.triangles.len(),
            budget: options.triangle_budget,
        });
    }
    let material_ids = geometry
        .material_names
        .iter()
        .map(|name| materials.id(name))
        .collect::<Result<Vec<_>>>()?;
    let mesh = AcousticMesh {
        vertices_enu_m: geometry.vertices_enu_m,
        triangles: geometry.triangles,
        material_ids,
    };
    mesh.validate(materials.iter().len(), options.triangle_budget)?;
    Ok(mesh)
}

fn subtract(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m - right.east_m,
        left.north_m - right.north_m,
        left.up_m - right.up_m,
    )
}

fn cross(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.north_m * right.up_m - left.up_m * right.north_m,
        left.up_m * right.east_m - left.east_m * right.up_m,
        left.east_m * right.north_m - left.north_m * right.east_m,
    )
}
