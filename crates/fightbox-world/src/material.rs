use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::{Result, WorldError};

/// Steam Audio-compatible three-band acoustic coefficients.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub absorption: [f32; 3],
    pub scattering: f32,
    pub transmission: [f32; 3],
}

impl Material {
    pub fn validate(&self, name: &str) -> Result<()> {
        if self
            .absorption
            .into_iter()
            .chain([self.scattering])
            .chain(self.transmission)
            .any(|coefficient| !coefficient.is_finite() || !(0.0..=1.0).contains(&coefficient))
        {
            return Err(WorldError::InvalidMaterial {
                name: name.to_owned(),
                reason: "coefficients must be finite values between zero and one",
            });
        }
        Ok(())
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "absorption": self.absorption,
            "scattering": self.scattering,
            "transmission": self.transmission,
        })
    }

    pub(crate) fn from_json(name: &str, value: &Value) -> Result<Self> {
        let object = value.as_object().ok_or_else(|| {
            WorldError::InvalidPackage(format!("material {name:?} must be an object"))
        })?;
        let absorption = coefficient_triplet(object.get("absorption"), name)?;
        let transmission = coefficient_triplet(object.get("transmission"), name)?;
        let scattering = object
            .get("scattering")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                WorldError::InvalidPackage(format!(
                    "material {name:?} is missing numeric scattering"
                ))
            })? as f32;
        let material = Self {
            absorption,
            scattering,
            transmission,
        };
        material.validate(name)?;
        Ok(material)
    }
}

fn coefficient_triplet(value: Option<&Value>, name: &str) -> Result<[f32; 3]> {
    let values = value.and_then(Value::as_array).ok_or_else(|| {
        WorldError::InvalidPackage(format!(
            "material {name:?} coefficient row must be an array"
        ))
    })?;
    if values.len() != 3 {
        return Err(WorldError::InvalidPackage(format!(
            "material {name:?} coefficient row must have three bands"
        )));
    }
    let mut result = [0.0; 3];
    for (destination, source) in result.iter_mut().zip(values) {
        *destination = source.as_f64().ok_or_else(|| {
            WorldError::InvalidPackage(format!("material {name:?} coefficients must be numeric"))
        })? as f32;
    }
    Ok(result)
}

/// A name-sorted material table. Its stable ordering defines serialized material IDs.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterialTable {
    entries: BTreeMap<String, Material>,
}

impl MaterialTable {
    #[must_use]
    pub fn new(entries: BTreeMap<String, Material>) -> Self {
        Self { entries }
    }

    pub fn validate(&self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(WorldError::InvalidPackage(
                "material table must not be empty".to_owned(),
            ));
        }
        for (name, material) in &self.entries {
            if name.trim().is_empty() {
                return Err(WorldError::InvalidMaterial {
                    name: name.clone(),
                    reason: "name must not be empty",
                });
            }
            material.validate(name)?;
        }
        Ok(())
    }

    pub fn id(&self, name: &str) -> Result<u32> {
        self.entries
            .keys()
            .position(|candidate| candidate == name)
            .and_then(|index| u32::try_from(index).ok())
            .ok_or_else(|| WorldError::UnknownMaterial(name.to_owned()))
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Material> {
        self.entries.get(name)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &Material)> {
        self.entries
            .iter()
            .map(|(name, material)| (name.as_str(), material))
    }

    pub(crate) fn to_json(&self) -> Value {
        let entries = self
            .entries
            .iter()
            .map(|(name, material)| (name.clone(), material.to_json()))
            .collect();
        Value::Object(entries)
    }

    pub(crate) fn from_json(value: &Value) -> Result<Self> {
        let object = value.as_object().ok_or_else(|| {
            WorldError::InvalidPackage("material table must be an object".to_owned())
        })?;
        let mut entries = BTreeMap::new();
        for (name, value) in object {
            entries.insert(name.clone(), Material::from_json(name, value)?);
        }
        let table = Self { entries };
        table.validate()?;
        Ok(table)
    }
}

impl Default for MaterialTable {
    fn default() -> Self {
        let entries = [
            (
                "asphalt",
                Material {
                    absorption: [0.02, 0.03, 0.04],
                    scattering: 0.08,
                    transmission: [0.0, 0.0, 0.0],
                },
            ),
            // Brick/concrete transmission uses the ratified masonry triple:
            // 32/45/55 dB transmission loss per band, expressed in Steam Audio
            // 4.8.1's amplitude-EQ convention (coefficient = 10^(-TL/20)), the
            // same NRC/OSHA-anchored values as the fixture masonry material.
            // Zero here made compiled-city walls fully opaque and silently
            // overrode fixture-authored transmission (the package material
            // table, not fixture.json, governs live workbench geometry).
            (
                "brick",
                Material {
                    absorption: [0.03, 0.04, 0.07],
                    scattering: 0.15,
                    transmission: [0.0251, 0.005_62, 0.001_78],
                },
            ),
            (
                "concrete",
                Material {
                    absorption: [0.02, 0.03, 0.05],
                    scattering: 0.1,
                    transmission: [0.0251, 0.005_62, 0.001_78],
                },
            ),
            (
                "glass",
                Material {
                    absorption: [0.08, 0.05, 0.03],
                    scattering: 0.05,
                    transmission: [0.12, 0.08, 0.04],
                },
            ),
            (
                "grass",
                Material {
                    absorption: [0.1, 0.35, 0.65],
                    scattering: 0.4,
                    transmission: [0.0, 0.0, 0.0],
                },
            ),
        ]
        .into_iter()
        .map(|(name, material)| (name.to_owned(), material))
        .collect();
        Self { entries }
    }
}

impl MaterialTable {
    /// The default table plus the city-detail materials: assessor facades,
    /// roofs and garages, LiDAR fences, and rail structures.
    ///
    /// Values follow the city-feel review (2026-10-01): facade scattering is
    /// a provisional 0.20 masonry / 0.35 porch-heavy frame, not a measured
    /// Chicago coefficient, and facade transmission mixes the opaque wall
    /// with ~25/30/35 dB glazing, as `TL = −10·log10[(1−g)·10^(−TLw/10) +
    /// g·10^(−TLg/10)]`. Transmission is amplitude `10^(−TL/20)`. Only
    /// packages whose features name one of these materials use this table, so
    /// every other package keeps the default table and its content hash.
    #[must_use]
    pub fn city_detail() -> Self {
        let mut entries = Self::default().entries;
        // 29/35.6/40.9 dB.
        let masonry = [0.035_48, 0.016_60, 0.009_016];
        // 25/36/41.5 dB, also the timber-backed roofs.
        let frame = [0.056_23, 0.015_85, 0.008_414];
        // 20/30/35 dB: an alley face dominated by its overhead door.
        let garage = [0.1, 0.031_62, 0.017_78];
        let opaque = [0.0, 0.0, 0.0];
        for (name, absorption, scattering, transmission) in [
            ("masonry_facade", [0.09, 0.06, 0.06], 0.20, masonry),
            ("frame_facade", [0.16, 0.06, 0.05], 0.35, frame),
            ("stucco_facade", [0.13, 0.05, 0.03], 0.25, frame),
            // Setbacks and garage-to-garage breaks are already geometry.
            ("garage_masonry", [0.06, 0.05, 0.06], 0.15, garage),
            ("garage_frame", [0.10, 0.06, 0.06], 0.15, garage),
            ("roof_shingle", [0.05, 0.06, 0.08], 0.10, frame),
            ("roof_gravel", [0.05, 0.10, 0.15], 0.20, frame),
            // Board fence, gaps at most ~3% of its area: 10/13/15 dB.
            (
                "wood_fence",
                [0.10, 0.08, 0.10],
                0.30,
                [0.316_2, 0.223_9, 0.177_8],
            ),
            ("ballast", [0.30, 0.60, 0.70], 0.30, opaque),
            // One elevated track's girders and ties as a strip, open between
            // tracks; ~10.5 dB stands in for the gaps between ties.
            ("rail_deck", [0.05, 0.05, 0.08], 0.30, [0.3, 0.3, 0.3]),
        ] {
            entries.insert(
                name.to_owned(),
                Material {
                    absorption,
                    scattering,
                    transmission,
                },
            );
        }
        Self { entries }
    }
}

#[cfg(test)]
mod tests {
    use super::MaterialTable;

    #[test]
    fn city_detail_keeps_every_default_material_unchanged() {
        let detail = MaterialTable::city_detail();
        detail.validate().unwrap();
        for (name, material) in MaterialTable::default().iter() {
            assert_eq!(detail.get(name), Some(material));
        }
        assert!(detail.get("wood_fence").is_some() && detail.get("ballast").is_some());
    }
}
