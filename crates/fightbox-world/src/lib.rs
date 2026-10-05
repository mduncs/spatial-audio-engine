//! City compiler: triangle providers, acoustic mesh generation and
//! validation, material table, and the `.fightbox` package format (§ο Phase C).

#![forbid(unsafe_code)]

mod city_route;
mod echo_authority;
mod echo_extractor;
mod echo_package;
mod error;
mod material;
mod mesh;
mod package;
mod package_v2;
mod probe_layout;
mod provider;
mod sha256;

pub use city_route::{
    CITY_ROUTE_ASSEMBLER_REVISION, CITY_ROUTE_MANIFEST_FILENAME, CITY_ROUTE_SCHEMA_ID,
    CityRouteAdjacency, CityRouteAdjacencyAxis, CityRouteAssemblyRequest, CityRouteBakeBinding,
    CityRouteBakeState, CityRouteCellInput, CityRouteCellRecord, CityRouteCompletedBakeBinding,
    CityRouteEchoAuthorityBinding, CityRouteEchoAuthorityInput, CityRouteGridPolicy,
    CityRouteInstalledSize, CityRouteInstalledTotals, CityRouteManifest, CityRoutePrefetchMetadata,
    CityRouteSelectionMetadata, CityRouteWorldBinding, FourCellFixturePlan, OwnerHomeDesignation,
    assemble_city_route,
};
pub use echo_authority::{
    AuthorityAdmission, ECHO_AUTHORITY_MAGIC, ECHO_AUTHORITY_SCHEMA_ID, ECHO_AUTHORITY_VERSION,
    EchoAuthorityBindings, EchoAuthorityDeclaration, EchoAuthorityError, EchoAuthorityFixture,
    EchoAuthorityQuery, EchoAuthorityTable, EchoPathKind, EchoPathRecord, EchoPathVertex, EchoPlan,
    EchoQueryTelemetry, EchoTapPlan, ExplicitNoPlanReason, ListenerNode, LoadedEchoAuthority,
    MAX_ECHO_CANDIDATES_PER_TILE, MAX_ECHO_TAPS_PER_PLAN, MOBILE_ECHO_AUTHORITY_CAP_BYTES,
    PlanTile, PlanTileContent, STATIC_ANCHOR_DOMAIN_TOLERANCE_M, Sha256Digest, StableGeometryKey,
    StablePathKey, StableSpatialKey, StaticSourceAnchor,
};
pub use echo_extractor::{
    DiffractionEdge, EchoAuthorityArtifactManifest, EchoExtractionArtifact, EchoExtractionBindings,
    EchoExtractionError, EchoExtractionRequest, EchoExtractionStats, EchoExtractor,
    EchoExtractorConfig, EchoListenerCoverage, EchoListenerSample, FacadePatch,
};
pub use echo_package::{
    ECHO_AUTHORITY_CAPABILITY, ECHO_AUTHORITY_SIDECAR_PATH, PackageEchoAuthority,
    echo_authority_extension_for_package, echo_coordinate_frame_key, echo_listener_cell_key,
    load_package_echo_authority,
};
pub use error::{Result, WorldError};
pub use material::{Material, MaterialTable};
pub use mesh::{AcousticMesh, CompileOptions, compile, export_obj};
pub use package::{
    FORMAT_VERSION, LoadedPackage, PackageManifest, PackageMetadata, Provenance, mesh_content_hash,
    package_manifest_bytes, package_manifest_content_hash,
    package_manifest_sha256_without_capability, read_package, read_package_with_capabilities,
    write_package, write_package_v2, write_package_v2_with_metadata, write_package_with_metadata,
};
pub use package_v2::{
    CELL_GEOMETRY_HALO_M, CELL_OWNERSHIP_GUARD_M, CELL_PAIRWISE_OVERLAP_M, CELL_PROBE_FOOTPRINT_M,
    CELL_STRIDE_M, CapabilityExtension, CellBounds, CellGridIndex, CellPayloadIndex,
    CellSupportedRanges, CellSwitchPlane, CityIdentity, CorePayloadReference, ExtensionRequirement,
    GeodeticOrigin, MOBILE_BAKED_PATH_HORIZON_M, MOBILE_HARD_RAW_PROBE_PAYLOAD_BYTES,
    MOBILE_TARGET_RAW_PROBE_PAYLOAD_BYTES, MobileCellPolicy, PackageCompression,
    STEAM_AUDIO_PROBE_BATCH_V1_CAPABILITY, WGS84_LOCAL_FRAME_ID, WORLD_MANIFEST_V2_FORMAT_VERSION,
    WORLD_MANIFEST_V2_SCHEMA_ID, WORLD_MANIFEST_V2_SCHEMA_JSON, WorldCellIndex,
    WorldManifestExtensionPolicy, WorldPackageV2Index, WorldPackageV2Metadata, stable_cell_id,
};
pub use probe_layout::{
    AboveMaximumLayerPolicy, AnalysisGridSet, AnalysisTile, AnalysisTileKind,
    CALIBRATED_MOBILE_PLACEMENT_POLICY_SHA256, CITY_BAKE_V2_CAPABILITY, CITY_BAKE_V2_SCHEMA_ID,
    CITY_BAKE_V2_SIDECAR_PATH, CalibratedProbeByteModelV2, CellProbeSlice, CityBakeV2ArtifactState,
    CityBakeV2BakedArtifact, CityBakeV2DeterministicTelemetry, CityBakeV2PathBakeSettings,
    CityBakeV2ProbeBatchIdentity, CityBakeV2ProbePlan, CityBoundsMm, ElevatedProbeLayerPolicy,
    FIXED_TIER_MESH_OPEN_PAIRS_ALGORITHM, FIXED_TIER_MESH_OPEN_PAIRS_COORDINATE_ENCODING,
    FIXED_TIER_MESH_OPEN_PAIRS_ESTIMATOR_REVISION, GradedProbePolicy, ListenerOwnership,
    MAXIMUM_PROBE_LAYER_M, MeshOpenPairObservable, PROBE_LAYOUT_ESTIMATOR_REVISION,
    ProbeByteEstimate, ProbeLayerSummary, ProbeSite, ProbeTierPolicy, ProbeTierSummary,
    ResolvedGradedProbeLayout, SkyPathingPolicy, calibrated_probe_byte_model,
    mesh_open_pair_observable, reachable_ordered_probe_pair_count, write_city_bake_v2_plan_sidecar,
};
pub use provider::{
    Assumption, GeoJsonOptions, GeoJsonProvider, ObjProvider, ProviderGeometry, TriangleProvider,
};
