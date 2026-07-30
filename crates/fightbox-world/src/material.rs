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
            (
                "brick",
                Material {
                    absorption: [0.03, 0.04, 0.07],
                    scattering: 0.15,
                    transmission: [0.0, 0.0, 0.0],
                },
            ),
            (
                "concrete",
                Material {
                    absorption: [0.02, 0.03, 0.05],
                    scattering: 0.1,
                    transmission: [0.0, 0.0, 0.0],
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
