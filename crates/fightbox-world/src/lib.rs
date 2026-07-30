//! City compiler: triangle providers, acoustic mesh generation and
//! validation, material table, and the `.fightbox` package format (§ο Phase C).

#![forbid(unsafe_code)]

mod error;
mod material;
mod mesh;
mod package;
mod provider;
mod sha256;

pub use error::{Result, WorldError};
pub use material::{Material, MaterialTable};
pub use mesh::{AcousticMesh, CompileOptions, compile, export_obj};
pub use package::{
    FORMAT_VERSION, LoadedPackage, PackageManifest, PackageMetadata, Provenance, mesh_content_hash,
    read_package, write_package, write_package_with_metadata,
};
pub use provider::{
    Assumption, GeoJsonOptions, GeoJsonProvider, ObjProvider, ProviderGeometry, TriangleProvider,
};
