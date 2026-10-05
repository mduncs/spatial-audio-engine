use std::{fmt, io, path::PathBuf};

pub type Result<T> = std::result::Result<T, WorldError>;

#[derive(Debug)]
pub enum WorldError {
    Io { path: PathBuf, source: io::Error },
    Json(String),
    InvalidGeoJson(String),
    InvalidObj(String),
    SelfIntersectingPolygon { feature: usize },
    TriangulationFailed { feature: usize },
    NonFiniteVertex { vertex: usize },
    DegenerateTriangle { triangle: usize },
    IndexOutOfRange { triangle: usize, index: u32 },
    MissingMaterialAssignment { triangle: usize },
    UnknownMaterial(String),
    InvalidMaterial { name: String, reason: &'static str },
    TriangleBudgetExceeded { actual: usize, budget: usize },
    InvalidPackage(String),
    HashMismatch { item: &'static str },
}

impl WorldError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for WorldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Json(message) => write!(f, "invalid JSON: {message}"),
            Self::InvalidGeoJson(message) => write!(f, "invalid GeoJSON: {message}"),
            Self::InvalidObj(message) => write!(f, "invalid OBJ: {message}"),
            Self::SelfIntersectingPolygon { feature } => {
                write!(
                    f,
                    "GeoJSON feature {feature} has a self-intersecting exterior ring"
                )
            }
            Self::TriangulationFailed { feature } => {
                write!(f, "could not triangulate GeoJSON feature {feature}")
            }
            Self::NonFiniteVertex { vertex } => write!(f, "vertex {vertex} is not finite"),
            Self::DegenerateTriangle { triangle } => {
                write!(f, "triangle {triangle} has zero area")
            }
            Self::IndexOutOfRange { triangle, index } => {
                write!(
                    f,
                    "triangle {triangle} references out-of-range vertex {index}"
                )
            }
            Self::MissingMaterialAssignment { triangle } => {
                write!(f, "triangle {triangle} has no material assignment")
            }
            Self::UnknownMaterial(name) => write!(f, "unknown material name {name:?}"),
            Self::InvalidMaterial { name, reason } => {
                write!(f, "invalid material {name:?}: {reason}")
            }
            Self::TriangleBudgetExceeded { actual, budget } => {
                write!(
                    f,
                    "triangle budget exceeded: generated {actual}, budget is {budget}"
                )
            }
            Self::InvalidPackage(message) => write!(f, "invalid .fightbox package: {message}"),
            Self::HashMismatch { item } => {
                write!(
                    f,
                    ".fightbox package {item} SHA-256 does not match manifest"
                )
            }
        }
    }
}

impl std::error::Error for WorldError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
