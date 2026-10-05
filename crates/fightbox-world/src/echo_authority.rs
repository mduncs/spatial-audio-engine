//! Deterministic offline mesh authority for V1 discrete event echoes.
//!
//! This module deliberately stops at geometry-derived tap plans. It never bakes
//! final sample delays, propagation gain, air absorption, or HRTF output.

use std::{collections::BTreeMap, fmt, mem::size_of};

use crate::sha256::sha256_hex;

pub const ECHO_AUTHORITY_SCHEMA_ID: &str = "fightbox.echo-authority.v1";
pub const ECHO_AUTHORITY_MAGIC: &[u8; 8] = b"FBXECHO\0";
pub const ECHO_AUTHORITY_VERSION: u16 = 1;
pub const STATIC_ANCHOR_DOMAIN_TOLERANCE_M: f32 = 0.020;
pub const MAX_ECHO_CANDIDATES_PER_TILE: usize = 12;
pub const MAX_ECHO_TAPS_PER_PLAN: usize = 4;
pub const MOBILE_ECHO_AUTHORITY_CAP_BYTES: usize = 64 * 1024 * 1024;

const MAX_SPECULAR_CANDIDATES_PER_TILE: usize = 8;
const MAX_DIFFRACTION_CANDIDATES_PER_TILE: usize = 4;
const MAX_TOTAL_PATH_M: f32 = 2_048.0;
const SPEED_OF_SOUND_M_S: f32 = 343.0;
const SPECULAR_MIN_EXCESS_M: f32 = 0.3 * SPEED_OF_SOUND_M_S;
const SPECULAR_MAX_EXCESS_M: f32 = 1.2 * SPEED_OF_SOUND_M_S;
const RELATIVE_PRUNE_PRESSURE_RATIO: f32 = 0.063_095_73; // -24 dB pressure.
// One-metre pressure law evaluated at 2,048 m, then the 9/15/24 dB V1
// corner losses. Kept as constants so trigger planning does not call `powf`.
const ABSOLUTE_PRUNE_PRESSURE_FLOOR: [f32; 3] =
    [0.000_173_248_72, 0.000_086_830_05, 0.000_030_808_46];
const CODEC_FIXED_HEADER_BYTES: usize = 252;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest(pub [u8; 32]);

impl Sha256Digest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(parse_sha256_hex(&sha256_hex(bytes)).expect("internal SHA-256 is lowercase hex"))
    }

    pub fn is_zero(self) -> bool {
        self.0 == [0; 32]
    }

    pub fn from_hex(value: &str) -> Result<Self, EchoAuthorityError> {
        parse_sha256_hex(value).map(Self).ok_or_else(|| {
            EchoAuthorityError::Malformed("SHA-256 must be 64 lowercase hex digits".to_owned())
        })
    }

    pub fn to_hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StableSpatialKey(pub [u8; 16]);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StableGeometryKey(pub [u8; 16]);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StablePathKey(pub [u8; 16]);

macro_rules! impl_stable_key {
    ($name:ident) => {
        impl $name {
            /// Derives an order-independent identity from caller-canonicalized bytes.
            pub fn derive(domain: &str, canonical_bytes: &[u8]) -> Self {
                let mut bytes = Vec::with_capacity(domain.len() + canonical_bytes.len() + 1);
                bytes.extend_from_slice(domain.as_bytes());
                bytes.push(0);
                bytes.extend_from_slice(canonical_bytes);
                let digest = Sha256Digest::from_bytes(&bytes);
                let mut key = [0; 16];
                key.copy_from_slice(&digest.0[..16]);
                Self(key)
            }

            pub fn is_zero(self) -> bool {
                self.0 == [0; 16]
            }

            pub fn to_hex(self) -> String {
                self.0.iter().map(|byte| format!("{byte:02x}")).collect()
            }
        }
    };
}

impl_stable_key!(StableSpatialKey);
impl_stable_key!(StableGeometryKey);
impl_stable_key!(StablePathKey);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EchoAuthorityBindings {
    pub package_manifest_hash: Sha256Digest,
    pub mesh_hash: Sha256Digest,
    pub material_hash: Sha256Digest,
    pub fixture_request_hash: Sha256Digest,
    pub anchor_set_hash: Sha256Digest,
    pub probe_layout_hash: Sha256Digest,
    pub coordinate_frame: StableSpatialKey,
    pub baker_revision: u32,
    pub sound_contract_revision: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StaticSourceAnchor {
    pub key: StableSpatialKey,
    pub position_city_enu_m: [f32; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListenerNode {
    pub key: StableSpatialKey,
    pub listener_cell: StableSpatialKey,
    pub position_city_enu_m: [f32; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EchoPathKind {
    Specular = 0,
    Diffraction = 1,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EchoPathVertex {
    pub total_path_m: f32,
    pub excess_path_m: f32,
    pub final_interaction_city_enu_m: [f32; 3],
    /// Cartesian vector from the final interaction toward this listener node.
    pub arrival_vector_city_enu_m: [f32; 3],
    /// Signed 250 Hz / 1 kHz / 4 kHz material pressure coefficients.
    pub material_pressure: [f32; 3],
    /// Non-negative authority score inputs. This is not final distance/air gain.
    pub predicted_received_pressure: [f32; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoPathRecord {
    pub path_key: StablePathKey,
    pub geometry_key: StableGeometryKey,
    pub material_key: StableGeometryKey,
    pub kind: EchoPathKind,
    /// One sample at each of the tile's three listener vertices.
    pub vertices: [EchoPathVertex; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ExplicitNoPlanReason {
    VisibilityDiscontinuity = 0,
    GeometryBoundary = 1,
    AuthoredGap = 2,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlanTileContent {
    Paths(Vec<EchoPathRecord>),
    NoPlan(ExplicitNoPlanReason),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanTile {
    pub key: StableSpatialKey,
    pub source_anchor: StableSpatialKey,
    pub listener_cell: StableSpatialKey,
    pub listener_vertices: [StableSpatialKey; 3],
    pub direct_occluded: bool,
    pub content: PlanTileContent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoAuthorityTable {
    bindings: EchoAuthorityBindings,
    anchors: Vec<StaticSourceAnchor>,
    listener_nodes: Vec<ListenerNode>,
    tiles: Vec<PlanTile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EchoAuthorityDeclaration {
    pub schema_id: String,
    pub table_sha256: Sha256Digest,
    pub table_bytes: u64,
    pub bindings: EchoAuthorityBindings,
}

impl EchoAuthorityDeclaration {
    pub fn for_table(table: &EchoAuthorityTable) -> Self {
        let bytes = table.encode();
        Self {
            schema_id: ECHO_AUTHORITY_SCHEMA_ID.to_owned(),
            table_sha256: Sha256Digest::from_bytes(&bytes),
            table_bytes: bytes.len() as u64,
            bindings: table.bindings.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityAdmission {
    Desktop,
    Mobile { retained_resident_bytes: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedEchoAuthority {
    table: EchoAuthorityTable,
    encoded_bytes: usize,
    resident_bytes: usize,
}

impl LoadedEchoAuthority {
    pub fn table(&self) -> &EchoAuthorityTable {
        &self.table
    }

    pub fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }

    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    pub fn into_table(self) -> EchoAuthorityTable {
        self.table
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EchoAuthorityQuery {
    pub anchor: StableSpatialKey,
    pub source_position_city_enu_m: [f32; 3],
    pub listener_cell: StableSpatialKey,
    pub listener_position_city_enu_m: [f32; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EchoQueryTelemetry {
    Ready,
    AnchorOutOfDomain,
    OutsideCoverage,
    ExplicitNoPlan(ExplicitNoPlanReason),
    ZeroValidCandidates,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoTapPlan {
    pub path_key: StablePathKey,
    pub geometry_key: StableGeometryKey,
    pub material_key: StableGeometryKey,
    pub kind: EchoPathKind,
    pub total_path_m: f32,
    pub excess_path_m: f32,
    pub final_interaction_city_enu_m: [f32; 3],
    pub arrival_direction_city_enu: [f32; 3],
    pub material_pressure: [f32; 3],
    pub predicted_received_pressure: [f32; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoPlan {
    pub telemetry: EchoQueryTelemetry,
    pub tile: Option<StableSpatialKey>,
    pub taps: Vec<EchoTapPlan>,
}

impl EchoPlan {
    fn silent(telemetry: EchoQueryTelemetry, tile: Option<StableSpatialKey>) -> Self {
        Self {
            telemetry,
            tile,
            taps: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EchoAuthorityError {
    MissingDeclaredPayload,
    WrongSchema { actual: String },
    Malformed(String),
    StaleBinding { field: &'static str },
    TableHashMismatch,
    TableSizeMismatch { declared: u64, actual: u64 },
    MissingEnabledAnchor(StableSpatialKey),
    MobileResidentCapExceeded { requested: usize, cap: usize },
}

impl fmt::Display for EchoAuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDeclaredPayload => write!(f, "declared echo authority payload is missing"),
            Self::WrongSchema { actual } => write!(
                f,
                "echo authority schema must be {ECHO_AUTHORITY_SCHEMA_ID}, got {actual:?}"
            ),
            Self::Malformed(message) => write!(f, "malformed echo authority: {message}"),
            Self::StaleBinding { field } => {
                write!(f, "echo authority has stale or mismatched {field} binding")
            }
            Self::TableHashMismatch => write!(f, "echo authority table SHA-256 mismatch"),
            Self::TableSizeMismatch { declared, actual } => write!(
                f,
                "echo authority table size mismatch: declared {declared}, actual {actual}"
            ),
            Self::MissingEnabledAnchor(key) => {
                write!(f, "enabled echo anchor {:02x?} is missing", key.0)
            }
            Self::MobileResidentCapExceeded { requested, cap } => write!(
                f,
                "mobile echo authority residency would be {requested} bytes, above {cap} byte cap"
            ),
        }
    }
}

impl std::error::Error for EchoAuthorityError {}

impl EchoAuthorityTable {
    pub fn new(
        bindings: EchoAuthorityBindings,
        mut anchors: Vec<StaticSourceAnchor>,
        mut listener_nodes: Vec<ListenerNode>,
        mut tiles: Vec<PlanTile>,
    ) -> Result<Self, EchoAuthorityError> {
        anchors.sort_by_key(|anchor| anchor.key);
        listener_nodes.sort_by_key(|node| node.key);
        for tile in &mut tiles {
            if let PlanTileContent::Paths(paths) = &mut tile.content {
                paths.sort_by_key(|path| path.path_key);
            }
        }
        tiles.sort_by_key(|tile| tile.key);
        let table = Self {
            bindings,
            anchors,
            listener_nodes,
            tiles,
        };
        table.validate()?;
        Ok(table)
    }

    pub fn bindings(&self) -> &EchoAuthorityBindings {
        &self.bindings
    }

    pub fn anchors(&self) -> &[StaticSourceAnchor] {
        &self.anchors
    }

    pub fn listener_nodes(&self) -> &[ListenerNode] {
        &self.listener_nodes
    }

    pub fn tiles(&self) -> &[PlanTile] {
        &self.tiles
    }

    /// Scene-load validation. Absence or ambiguity cannot silently disable an enabled anchor.
    pub fn require_enabled_anchor(
        &self,
        key: StableSpatialKey,
    ) -> Result<&StaticSourceAnchor, EchoAuthorityError> {
        self.anchors
            .binary_search_by_key(&key, |anchor| anchor.key)
            .ok()
            .map(|index| &self.anchors[index])
            .ok_or(EchoAuthorityError::MissingEnabledAnchor(key))
    }

    pub fn query(&self, query: EchoAuthorityQuery) -> Result<EchoPlan, EchoAuthorityError> {
        let anchor = self.require_enabled_anchor(query.anchor)?;
        if !finite3(query.source_position_city_enu_m)
            || !finite3(query.listener_position_city_enu_m)
        {
            return Err(EchoAuthorityError::Malformed(
                "query positions must be finite".to_owned(),
            ));
        }
        if distance(anchor.position_city_enu_m, query.source_position_city_enu_m)
            > STATIC_ANCHOR_DOMAIN_TOLERANCE_M
        {
            return Ok(EchoPlan::silent(
                EchoQueryTelemetry::AnchorOutOfDomain,
                None,
            ));
        }

        let mut containing = Vec::new();
        for tile in self.tiles.iter().filter(|tile| {
            tile.source_anchor == query.anchor && tile.listener_cell == query.listener_cell
        }) {
            let triangle = tile.listener_vertices.map(|key| {
                self.listener_nodes
                    .binary_search_by_key(&key, |node| node.key)
                    .ok()
                    .map(|index| &self.listener_nodes[index])
                    .expect("validated listener node reference")
                    .position_city_enu_m
            });
            if let Some(weights) =
                barycentric_with_boundary_tolerance(query.listener_position_city_enu_m, triangle)
            {
                containing.push((tile, weights));
            }
        }
        if containing.is_empty() {
            return Ok(EchoPlan::silent(EchoQueryTelemetry::OutsideCoverage, None));
        }
        // Explicit discontinuities dominate overlapping boundary triangles.
        if let Some((tile, reason)) = containing.iter().find_map(|(tile, _)| match tile.content {
            PlanTileContent::NoPlan(reason) => Some((*tile, reason)),
            PlanTileContent::Paths(_) => None,
        }) {
            return Ok(EchoPlan::silent(
                EchoQueryTelemetry::ExplicitNoPlan(reason),
                Some(tile.key),
            ));
        }
        containing.sort_by_key(|(tile, _)| tile.key);
        let (tile, weights) = containing[0];
        let PlanTileContent::Paths(paths) = &tile.content else {
            unreachable!("explicit no-plan handled above")
        };
        let mut candidates: Vec<_> = paths
            .iter()
            .map(|path| interpolate_path(path, weights))
            .collect();
        prune_and_rank(&mut candidates, tile.direct_occluded);
        if candidates.is_empty() {
            return Ok(EchoPlan::silent(
                EchoQueryTelemetry::ZeroValidCandidates,
                Some(tile.key),
            ));
        }
        Ok(EchoPlan {
            telemetry: EchoQueryTelemetry::Ready,
            tile: Some(tile.key),
            taps: candidates,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(ECHO_AUTHORITY_MAGIC);
        write_u16(&mut out, ECHO_AUTHORITY_VERSION);
        write_u16(&mut out, 0);
        for digest in binding_digests(&self.bindings) {
            out.extend_from_slice(&digest.0);
        }
        out.extend_from_slice(&self.bindings.coordinate_frame.0);
        write_u32(&mut out, self.bindings.baker_revision);
        write_u32(&mut out, self.bindings.sound_contract_revision);
        write_f32(&mut out, SPEED_OF_SOUND_M_S);
        write_f32(&mut out, STATIC_ANCHOR_DOMAIN_TOLERANCE_M);
        write_f32(&mut out, MAX_TOTAL_PATH_M);
        write_u32(&mut out, self.anchors.len() as u32);
        write_u32(&mut out, self.listener_nodes.len() as u32);
        write_u32(&mut out, self.tiles.len() as u32);
        for anchor in &self.anchors {
            out.extend_from_slice(&anchor.key.0);
            write_vec3(&mut out, anchor.position_city_enu_m);
        }
        for node in &self.listener_nodes {
            out.extend_from_slice(&node.key.0);
            out.extend_from_slice(&node.listener_cell.0);
            write_vec3(&mut out, node.position_city_enu_m);
        }
        for tile in &self.tiles {
            out.extend_from_slice(&tile.key.0);
            out.extend_from_slice(&tile.source_anchor.0);
            out.extend_from_slice(&tile.listener_cell.0);
            for vertex in tile.listener_vertices {
                out.extend_from_slice(&vertex.0);
            }
            out.push(u8::from(tile.direct_occluded));
            match &tile.content {
                PlanTileContent::NoPlan(reason) => {
                    out.push(1);
                    out.push(*reason as u8);
                    out.push(0);
                    write_u32(&mut out, 0);
                }
                PlanTileContent::Paths(paths) => {
                    out.push(0);
                    out.extend_from_slice(&[0, 0]);
                    write_u32(&mut out, paths.len() as u32);
                    for path in paths {
                        encode_path(&mut out, path);
                    }
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, EchoAuthorityError> {
        let mut reader = Reader::new(bytes);
        if reader.take(8)? != ECHO_AUTHORITY_MAGIC {
            return Err(EchoAuthorityError::Malformed(
                "binary magic must be FBXECHO\\0".to_owned(),
            ));
        }
        if reader.u16()? != ECHO_AUTHORITY_VERSION {
            return Err(EchoAuthorityError::Malformed(
                "unsupported binary version".to_owned(),
            ));
        }
        if reader.u16()? != 0 {
            return Err(EchoAuthorityError::Malformed(
                "reserved header field is nonzero".to_owned(),
            ));
        }
        let bindings = EchoAuthorityBindings {
            package_manifest_hash: reader.digest()?,
            mesh_hash: reader.digest()?,
            material_hash: reader.digest()?,
            fixture_request_hash: reader.digest()?,
            anchor_set_hash: reader.digest()?,
            probe_layout_hash: reader.digest()?,
            coordinate_frame: reader.spatial_key()?,
            baker_revision: reader.u32()?,
            sound_contract_revision: reader.u32()?,
        };
        require_constant(reader.f32()?, SPEED_OF_SOUND_M_S, "speed of sound")?;
        require_constant(
            reader.f32()?,
            STATIC_ANCHOR_DOMAIN_TOLERANCE_M,
            "anchor tolerance",
        )?;
        require_constant(reader.f32()?, MAX_TOTAL_PATH_M, "maximum total path")?;
        let anchor_count = reader.count("anchor", 28)?;
        let node_count = reader.count("listener node", 44)?;
        let tile_count = reader.count("plan tile", 104)?;
        let mut anchors = Vec::with_capacity(anchor_count);
        for _ in 0..anchor_count {
            anchors.push(StaticSourceAnchor {
                key: reader.spatial_key()?,
                position_city_enu_m: reader.vec3()?,
            });
        }
        let mut listener_nodes = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            listener_nodes.push(ListenerNode {
                key: reader.spatial_key()?,
                listener_cell: reader.spatial_key()?,
                position_city_enu_m: reader.vec3()?,
            });
        }
        let mut tiles = Vec::with_capacity(tile_count);
        for _ in 0..tile_count {
            let key = reader.spatial_key()?;
            let source_anchor = reader.spatial_key()?;
            let listener_cell = reader.spatial_key()?;
            let listener_vertices = [
                reader.spatial_key()?,
                reader.spatial_key()?,
                reader.spatial_key()?,
            ];
            let direct_occluded = reader.bool()?;
            let content_tag = reader.u8()?;
            let reason = reader.u8()?;
            if reader.u8()? != 0 {
                return Err(EchoAuthorityError::Malformed(
                    "reserved tile field is nonzero".to_owned(),
                ));
            }
            let path_count = reader.u32()? as usize;
            let content = match content_tag {
                0 => {
                    if reason != 0 || path_count > MAX_ECHO_CANDIDATES_PER_TILE {
                        return Err(EchoAuthorityError::Malformed(
                            "invalid path tile header".to_owned(),
                        ));
                    }
                    let mut paths = Vec::with_capacity(path_count);
                    for _ in 0..path_count {
                        paths.push(decode_path(&mut reader)?);
                    }
                    PlanTileContent::Paths(paths)
                }
                1 => {
                    if path_count != 0 {
                        return Err(EchoAuthorityError::Malformed(
                            "no-plan tile contains paths".to_owned(),
                        ));
                    }
                    PlanTileContent::NoPlan(match reason {
                        0 => ExplicitNoPlanReason::VisibilityDiscontinuity,
                        1 => ExplicitNoPlanReason::GeometryBoundary,
                        2 => ExplicitNoPlanReason::AuthoredGap,
                        _ => {
                            return Err(EchoAuthorityError::Malformed(
                                "unknown no-plan reason".to_owned(),
                            ));
                        }
                    })
                }
                _ => {
                    return Err(EchoAuthorityError::Malformed(
                        "unknown plan tile content tag".to_owned(),
                    ));
                }
            };
            tiles.push(PlanTile {
                key,
                source_anchor,
                listener_cell,
                listener_vertices,
                direct_occluded,
                content,
            });
        }
        if !reader.is_empty() {
            return Err(EchoAuthorityError::Malformed(
                "trailing bytes after authority table".to_owned(),
            ));
        }
        let table = Self::new(bindings, anchors, listener_nodes, tiles)?;
        if table.encode() != bytes {
            return malformed("table records are not in canonical order");
        }
        Ok(table)
    }

    pub fn load_declared(
        bytes: Option<&[u8]>,
        declaration: &EchoAuthorityDeclaration,
        admission: AuthorityAdmission,
    ) -> Result<LoadedEchoAuthority, EchoAuthorityError> {
        if declaration.schema_id != ECHO_AUTHORITY_SCHEMA_ID {
            return Err(EchoAuthorityError::WrongSchema {
                actual: declaration.schema_id.clone(),
            });
        }
        let bytes = bytes.ok_or(EchoAuthorityError::MissingDeclaredPayload)?;
        if declaration.table_bytes != bytes.len() as u64 {
            return Err(EchoAuthorityError::TableSizeMismatch {
                declared: declaration.table_bytes,
                actual: bytes.len() as u64,
            });
        }
        if Sha256Digest::from_bytes(bytes) != declaration.table_sha256 {
            return Err(EchoAuthorityError::TableHashMismatch);
        }
        let table = Self::decode(bytes)?;
        check_bindings(&table.bindings, &declaration.bindings)?;
        let resident_bytes = table.resident_bytes();
        if let AuthorityAdmission::Mobile {
            retained_resident_bytes,
        } = admission
        {
            let requested = retained_resident_bytes.checked_add(resident_bytes).ok_or(
                EchoAuthorityError::MobileResidentCapExceeded {
                    requested: usize::MAX,
                    cap: MOBILE_ECHO_AUTHORITY_CAP_BYTES,
                },
            )?;
            if requested > MOBILE_ECHO_AUTHORITY_CAP_BYTES {
                return Err(EchoAuthorityError::MobileResidentCapExceeded {
                    requested,
                    cap: MOBILE_ECHO_AUTHORITY_CAP_BYTES,
                });
            }
        }
        Ok(LoadedEchoAuthority {
            table,
            encoded_bytes: bytes.len(),
            resident_bytes,
        })
    }

    pub fn resident_bytes(&self) -> usize {
        let path_count: usize = self
            .tiles
            .iter()
            .map(|tile| match &tile.content {
                PlanTileContent::Paths(paths) => paths.capacity(),
                PlanTileContent::NoPlan(_) => 0,
            })
            .sum();
        size_of::<Self>()
            + self.anchors.capacity() * size_of::<StaticSourceAnchor>()
            + self.listener_nodes.capacity() * size_of::<ListenerNode>()
            + self.tiles.capacity() * size_of::<PlanTile>()
            + path_count * size_of::<EchoPathRecord>()
    }

    fn encoded_len(&self) -> usize {
        CODEC_FIXED_HEADER_BYTES
            + self.anchors.len() * 28
            + self.listener_nodes.len() * 44
            + self
                .tiles
                .iter()
                .map(|tile| {
                    104 + match &tile.content {
                        PlanTileContent::Paths(paths) => paths.len() * encoded_path_bytes(),
                        PlanTileContent::NoPlan(_) => 0,
                    }
                })
                .sum::<usize>()
    }

    fn validate(&self) -> Result<(), EchoAuthorityError> {
        for (field, digest) in [
            ("package manifest", self.bindings.package_manifest_hash),
            ("mesh", self.bindings.mesh_hash),
            ("material", self.bindings.material_hash),
            ("fixture/request", self.bindings.fixture_request_hash),
            ("anchor set", self.bindings.anchor_set_hash),
            ("probe layout", self.bindings.probe_layout_hash),
        ] {
            if digest.is_zero() {
                return malformed(format!("{field} hash must not be zero"));
            }
        }
        if self.bindings.coordinate_frame.is_zero() {
            return malformed("coordinate-frame key must not be zero");
        }
        if self.bindings.baker_revision == 0 || self.bindings.sound_contract_revision == 0 {
            return malformed("baker and sound-contract revisions must be nonzero");
        }
        ensure_unique_sorted(
            self.anchors.iter().map(|anchor| anchor.key),
            "source anchor key",
        )?;
        ensure_unique_sorted(
            self.listener_nodes.iter().map(|node| node.key),
            "listener node key",
        )?;
        ensure_unique_sorted(self.tiles.iter().map(|tile| tile.key), "plan tile key")?;
        for anchor in &self.anchors {
            if anchor.key.is_zero() || !finite3(anchor.position_city_enu_m) {
                return malformed("source anchor has zero identity or non-finite position");
            }
        }
        let anchors: BTreeMap<_, _> = self.anchors.iter().map(|a| (a.key, a)).collect();
        let nodes: BTreeMap<_, _> = self
            .listener_nodes
            .iter()
            .map(|node| (node.key, node))
            .collect();
        for node in &self.listener_nodes {
            if node.key.is_zero()
                || node.listener_cell.is_zero()
                || !finite3(node.position_city_enu_m)
            {
                return malformed("listener node has zero identity or non-finite position");
            }
        }
        for tile in &self.tiles {
            if tile.key.is_zero() || !anchors.contains_key(&tile.source_anchor) {
                return malformed("plan tile has zero identity or unknown source anchor");
            }
            let triangle = tile.listener_vertices.map(|key| {
                nodes
                    .get(&key)
                    .copied()
                    .ok_or_else(|| EchoAuthorityError::Malformed("unknown listener node".into()))
            });
            let [a, b, c] = triangle;
            let [a, b, c] = [a?, b?, c?];
            if [a, b, c]
                .iter()
                .any(|node| node.listener_cell != tile.listener_cell)
            {
                return malformed("plan tile crosses listener cells");
            }
            if triangle_area_twice(
                a.position_city_enu_m,
                b.position_city_enu_m,
                c.position_city_enu_m,
            ) <= 1.0e-6
            {
                return malformed("plan tile listener triangle is degenerate");
            }
            if let PlanTileContent::Paths(paths) = &tile.content {
                if paths.len() > MAX_ECHO_CANDIDATES_PER_TILE {
                    return malformed("plan tile exceeds 12 candidate limit");
                }
                let specular = paths
                    .iter()
                    .filter(|path| path.kind == EchoPathKind::Specular)
                    .count();
                let diffraction = paths.len() - specular;
                if specular > MAX_SPECULAR_CANDIDATES_PER_TILE
                    || diffraction > MAX_DIFFRACTION_CANDIDATES_PER_TILE
                {
                    return malformed("plan tile exceeds per-kind candidate limits");
                }
                ensure_unique_sorted(paths.iter().map(|path| path.path_key), "path key")?;
                for path in paths {
                    validate_path(path)?;
                }
            }
        }
        Ok(())
    }
}

/// Controlled deterministic constructors used by table tooling and deleted-geometry smokes.
pub struct EchoAuthorityFixture;

impl EchoAuthorityFixture {
    pub fn facade(include_facade: bool) -> Result<EchoAuthorityTable, EchoAuthorityError> {
        controlled_fixture(false, include_facade)
    }

    pub fn corner(include_edge: bool) -> Result<EchoAuthorityTable, EchoAuthorityError> {
        controlled_fixture(true, include_edge)
    }
}

fn controlled_fixture(
    direct_occluded: bool,
    include_geometry: bool,
) -> Result<EchoAuthorityTable, EchoAuthorityError> {
    let fixture_name = if direct_occluded { "corner" } else { "facade" };
    let key = |kind: &str, value: &str| {
        StableSpatialKey::derive(kind, format!("{fixture_name}:{value}").as_bytes())
    };
    let anchor = key("echo-anchor", "source");
    let cell = key("listener-cell", "cell-0");
    let node_keys = [
        key("listener-node", "a"),
        key("listener-node", "b"),
        key("listener-node", "c"),
    ];
    let positions = [[0.0, 0.0, 1.5], [8.0, 0.0, 1.5], [0.0, 8.0, 1.5]];
    let geometry = StableGeometryKey::derive("echo-geometry", fixture_name.as_bytes());
    let material = StableGeometryKey::derive("echo-material", b"controlled-concrete");
    let path = StablePathKey::derive("echo-path", fixture_name.as_bytes());
    let kind = if direct_occluded {
        EchoPathKind::Diffraction
    } else {
        EchoPathKind::Specular
    };
    let paths = if include_geometry {
        vec![EchoPathRecord {
            path_key: path,
            geometry_key: geometry,
            material_key: material,
            kind,
            vertices: positions.map(|listener| {
                let interaction = [20.0, 12.0, 4.0];
                EchoPathVertex {
                    total_path_m: if direct_occluded { 160.0 } else { 180.0 },
                    excess_path_m: if direct_occluded { 36.0 } else { 120.0 },
                    final_interaction_city_enu_m: interaction,
                    arrival_vector_city_enu_m: subtract(listener, interaction),
                    material_pressure: if direct_occluded {
                        [0.354_813_4, 0.177_827_94, 0.063_095_73]
                    } else {
                        [0.82, 0.74, 0.58]
                    },
                    predicted_received_pressure: if direct_occluded {
                        [0.0022, 0.0011, 0.0004]
                    } else {
                        [0.0045, 0.0040, 0.0031]
                    },
                }
            }),
        }]
    } else {
        Vec::new()
    };
    EchoAuthorityTable::new(
        fixture_bindings(fixture_name),
        vec![StaticSourceAnchor {
            key: anchor,
            position_city_enu_m: [40.0, -10.0, 1.5],
        }],
        node_keys
            .into_iter()
            .zip(positions)
            .map(|(key, position_city_enu_m)| ListenerNode {
                key,
                listener_cell: cell,
                position_city_enu_m,
            })
            .collect(),
        vec![PlanTile {
            key: key("plan-tile", "tile-0"),
            source_anchor: anchor,
            listener_cell: cell,
            listener_vertices: node_keys,
            direct_occluded,
            content: PlanTileContent::Paths(paths),
        }],
    )
}

fn fixture_bindings(name: &str) -> EchoAuthorityBindings {
    let digest = |field: &str| Sha256Digest::from_bytes(format!("{name}:{field}").as_bytes());
    EchoAuthorityBindings {
        package_manifest_hash: digest("package"),
        mesh_hash: digest("mesh"),
        material_hash: digest("material"),
        fixture_request_hash: digest("request"),
        anchor_set_hash: digest("anchors"),
        probe_layout_hash: digest("layout"),
        coordinate_frame: StableSpatialKey::derive("coordinate-frame", b"city-enu-mm-v1"),
        baker_revision: 1,
        sound_contract_revision: 1,
    }
}

fn validate_path(path: &EchoPathRecord) -> Result<(), EchoAuthorityError> {
    if path.path_key.is_zero() || path.geometry_key.is_zero() || path.material_key.is_zero() {
        return malformed("path has zero stable identity");
    }
    for vertex in path.vertices {
        if !vertex.total_path_m.is_finite()
            || vertex.total_path_m <= 0.0
            || vertex.total_path_m > MAX_TOTAL_PATH_M
            || !vertex.excess_path_m.is_finite()
            || vertex.excess_path_m < 0.0
            || vertex.excess_path_m > vertex.total_path_m
            || !finite3(vertex.final_interaction_city_enu_m)
            || !finite3(vertex.arrival_vector_city_enu_m)
            || length(vertex.arrival_vector_city_enu_m) <= 1.0e-6
            || !finite3(vertex.material_pressure)
            || vertex
                .material_pressure
                .iter()
                .any(|value| value.abs() > 1.0)
            || !finite3(vertex.predicted_received_pressure)
            || vertex
                .predicted_received_pressure
                .iter()
                .any(|value| *value < 0.0)
        {
            return malformed("path vertex is outside the V1 numeric domain");
        }
        if path.kind == EchoPathKind::Specular
            && !(SPECULAR_MIN_EXCESS_M..=SPECULAR_MAX_EXCESS_M).contains(&vertex.excess_path_m)
        {
            return malformed("specular path is outside the 0.3-1.2 s excess window");
        }
    }
    Ok(())
}

fn interpolate_path(path: &EchoPathRecord, weights: [f32; 3]) -> EchoTapPlan {
    let scalar = |field: fn(&EchoPathVertex) -> f32| {
        path.vertices
            .iter()
            .zip(weights)
            .map(|(vertex, weight)| field(vertex) * weight)
            .sum()
    };
    let vector = |field: fn(&EchoPathVertex) -> [f32; 3]| {
        std::array::from_fn(|axis| {
            path.vertices
                .iter()
                .zip(weights)
                .map(|(vertex, weight)| field(vertex)[axis] * weight)
                .sum()
        })
    };
    EchoTapPlan {
        path_key: path.path_key,
        geometry_key: path.geometry_key,
        material_key: path.material_key,
        kind: path.kind,
        total_path_m: scalar(|v| v.total_path_m),
        excess_path_m: scalar(|v| v.excess_path_m),
        final_interaction_city_enu_m: vector(|v| v.final_interaction_city_enu_m),
        arrival_direction_city_enu: normalize(vector(|v| v.arrival_vector_city_enu_m)),
        material_pressure: vector(|v| v.material_pressure),
        predicted_received_pressure: vector(|v| v.predicted_received_pressure),
    }
}

fn prune_and_rank(candidates: &mut Vec<EchoTapPlan>, direct_occluded: bool) {
    let strongest = |kind| {
        candidates
            .iter()
            .filter(|tap| tap.kind == kind)
            .fold([0.0_f32; 3], |mut maximum, tap| {
                for band in 0..3 {
                    maximum[band] = maximum[band].max(tap.predicted_received_pressure[band]);
                }
                maximum
            })
    };
    let specular_max = strongest(EchoPathKind::Specular);
    let diffraction_max = strongest(EchoPathKind::Diffraction);
    candidates.retain(|tap| {
        if tap.kind == EchoPathKind::Diffraction && !direct_occluded {
            return false;
        }
        let maximum = match tap.kind {
            EchoPathKind::Specular => specular_max,
            EchoPathKind::Diffraction => diffraction_max,
        };
        let relatively_inaudible = (0..3).all(|band| {
            tap.predicted_received_pressure[band] < maximum[band] * RELATIVE_PRUNE_PRESSURE_RATIO
        });
        let below_absolute_floor = (0..3).all(|band| {
            tap.predicted_received_pressure[band] < ABSOLUTE_PRUNE_PRESSURE_FLOOR[band]
        });
        !(relatively_inaudible && below_absolute_floor)
    });
    candidates.sort_by(compare_taps);
    let mut specular = 0;
    let mut diffraction = 0;
    candidates.retain(|tap| match tap.kind {
        EchoPathKind::Specular if specular < 3 => {
            specular += 1;
            true
        }
        EchoPathKind::Diffraction if direct_occluded && diffraction < 1 => {
            diffraction += 1;
            true
        }
        _ => false,
    });
    candidates.sort_by(compare_taps);
    debug_assert!(candidates.len() <= MAX_ECHO_TAPS_PER_PLAN);
}

pub(crate) fn prune_bake_candidates(candidates: &mut Vec<EchoPathRecord>) {
    let average_pressure = |path: &EchoPathRecord| {
        std::array::from_fn::<_, 3, _>(|band| {
            path.vertices
                .iter()
                .map(|vertex| vertex.predicted_received_pressure[band])
                .sum::<f32>()
                / 3.0
        })
    };
    let strongest = |kind| {
        candidates.iter().filter(|path| path.kind == kind).fold(
            [0.0_f32; 3],
            |mut maximum, path| {
                let pressure = average_pressure(path);
                for band in 0..3 {
                    maximum[band] = maximum[band].max(pressure[band]);
                }
                maximum
            },
        )
    };
    let maxima = [
        strongest(EchoPathKind::Specular),
        strongest(EchoPathKind::Diffraction),
    ];
    candidates.retain(|path| {
        let pressure = average_pressure(path);
        let maximum = maxima[path.kind as usize];
        let relatively_inaudible =
            (0..3).all(|band| pressure[band] < maximum[band] * RELATIVE_PRUNE_PRESSURE_RATIO);
        let below_absolute_floor =
            (0..3).all(|band| pressure[band] < ABSOLUTE_PRUNE_PRESSURE_FLOOR[band]);
        !(relatively_inaudible && below_absolute_floor)
    });
    candidates.sort_by(|left, right| {
        let score = |path: &EchoPathRecord| {
            let pressure = average_pressure(path);
            pressure[0] * 0.2 + pressure[1] * 0.5 + pressure[2] * 0.3
        };
        score(right)
            .total_cmp(&score(left))
            .then_with(|| {
                let total = |path: &EchoPathRecord| {
                    path.vertices
                        .iter()
                        .map(|vertex| vertex.total_path_m)
                        .sum::<f32>()
                        / 3.0
                };
                total(left).total_cmp(&total(right))
            })
            .then_with(|| left.path_key.cmp(&right.path_key))
    });
    let mut specular = 0;
    let mut diffraction = 0;
    candidates.retain(|path| match path.kind {
        EchoPathKind::Specular if specular < MAX_SPECULAR_CANDIDATES_PER_TILE => {
            specular += 1;
            true
        }
        EchoPathKind::Diffraction if diffraction < MAX_DIFFRACTION_CANDIDATES_PER_TILE => {
            diffraction += 1;
            true
        }
        _ => false,
    });
    candidates.sort_by_key(|path| path.path_key);
}

fn compare_taps(left: &EchoTapPlan, right: &EchoTapPlan) -> std::cmp::Ordering {
    tap_score(right)
        .total_cmp(&tap_score(left))
        .then_with(|| left.total_path_m.total_cmp(&right.total_path_m))
        .then_with(|| left.path_key.cmp(&right.path_key))
}

fn tap_score(tap: &EchoTapPlan) -> f32 {
    let pressure = tap.predicted_received_pressure;
    pressure[0] * 0.2 + pressure[1] * 0.5 + pressure[2] * 0.3
}

fn barycentric_with_boundary_tolerance(
    point: [f32; 3],
    triangle: [[f32; 3]; 3],
) -> Option<[f32; 3]> {
    let [a, b, c] = triangle;
    let v0 = subtract(b, a);
    let v1 = subtract(c, a);
    let v2 = subtract(point, a);
    let d00 = dot(v0, v0) as f64;
    let d01 = dot(v0, v1) as f64;
    let d11 = dot(v1, v1) as f64;
    let d20 = dot(v2, v0) as f64;
    let d21 = dot(v2, v1) as f64;
    let denominator = d00 * d11 - d01 * d01;
    if denominator <= 1.0e-12 {
        return None;
    }
    let v = (d11 * d20 - d01 * d21) / denominator;
    let w = (d00 * d21 - d01 * d20) / denominator;
    let u = 1.0 - v - w;
    let normal = cross(v0, v1);
    let plane_distance = (dot(v2, normal).abs() / length(normal)).abs();
    if plane_distance > STATIC_ANCHOR_DOMAIN_TOLERANCE_M {
        return None;
    }
    let shortest_edge = distance(a, b).min(distance(b, c)).min(distance(c, a));
    let tolerance = f64::from(STATIC_ANCHOR_DOMAIN_TOLERANCE_M / shortest_edge.max(0.001));
    if [u, v, w]
        .iter()
        .any(|weight| *weight < -tolerance || *weight > 1.0 + tolerance)
    {
        return None;
    }
    let mut weights = [u.max(0.0) as f32, v.max(0.0) as f32, w.max(0.0) as f32];
    let sum: f32 = weights.iter().sum();
    for weight in &mut weights {
        *weight /= sum;
    }
    Some(weights)
}

fn check_bindings(
    actual: &EchoAuthorityBindings,
    expected: &EchoAuthorityBindings,
) -> Result<(), EchoAuthorityError> {
    for (field, matches) in [
        (
            "package-manifest hash",
            actual.package_manifest_hash == expected.package_manifest_hash,
        ),
        ("mesh hash", actual.mesh_hash == expected.mesh_hash),
        (
            "material hash",
            actual.material_hash == expected.material_hash,
        ),
        (
            "fixture/request hash",
            actual.fixture_request_hash == expected.fixture_request_hash,
        ),
        (
            "anchor-set hash",
            actual.anchor_set_hash == expected.anchor_set_hash,
        ),
        (
            "probe-layout hash",
            actual.probe_layout_hash == expected.probe_layout_hash,
        ),
        (
            "coordinate frame",
            actual.coordinate_frame == expected.coordinate_frame,
        ),
        (
            "baker revision",
            actual.baker_revision == expected.baker_revision,
        ),
        (
            "sound-contract revision",
            actual.sound_contract_revision == expected.sound_contract_revision,
        ),
    ] {
        if !matches {
            return Err(EchoAuthorityError::StaleBinding { field });
        }
    }
    Ok(())
}

fn binding_digests(bindings: &EchoAuthorityBindings) -> [Sha256Digest; 6] {
    [
        bindings.package_manifest_hash,
        bindings.mesh_hash,
        bindings.material_hash,
        bindings.fixture_request_hash,
        bindings.anchor_set_hash,
        bindings.probe_layout_hash,
    ]
}

fn ensure_unique_sorted<T: Ord + Copy>(
    values: impl Iterator<Item = T>,
    label: &str,
) -> Result<(), EchoAuthorityError> {
    let mut previous = None;
    for value in values {
        if previous.is_some_and(|previous| previous >= value) {
            return malformed(format!("{label}s must be unique and canonically sorted"));
        }
        previous = Some(value);
    }
    Ok(())
}

fn require_constant(actual: f32, expected: f32, label: &str) -> Result<(), EchoAuthorityError> {
    if actual.to_bits() != expected.to_bits() {
        return malformed(format!("{label} does not match the V1 sound contract"));
    }
    Ok(())
}

fn malformed<T>(message: impl Into<String>) -> Result<T, EchoAuthorityError> {
    Err(EchoAuthorityError::Malformed(message.into()))
}

fn parse_sha256_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut output = [0; 32];
    for (destination, pair) in output.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *destination = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Some(output)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn encode_path(out: &mut Vec<u8>, path: &EchoPathRecord) {
    out.extend_from_slice(&path.path_key.0);
    out.extend_from_slice(&path.geometry_key.0);
    out.extend_from_slice(&path.material_key.0);
    out.push(path.kind as u8);
    out.extend_from_slice(&[0, 0, 0]);
    for vertex in path.vertices {
        write_f32(out, vertex.total_path_m);
        write_f32(out, vertex.excess_path_m);
        write_vec3(out, vertex.final_interaction_city_enu_m);
        write_vec3(out, vertex.arrival_vector_city_enu_m);
        write_vec3(out, vertex.material_pressure);
        write_vec3(out, vertex.predicted_received_pressure);
    }
}

fn decode_path(reader: &mut Reader<'_>) -> Result<EchoPathRecord, EchoAuthorityError> {
    let path_key = reader.path_key()?;
    let geometry_key = reader.geometry_key()?;
    let material_key = reader.geometry_key()?;
    let kind = match reader.u8()? {
        0 => EchoPathKind::Specular,
        1 => EchoPathKind::Diffraction,
        _ => return malformed("unknown echo path kind"),
    };
    if reader.take(3)? != [0, 0, 0] {
        return malformed("reserved path bytes are nonzero");
    }
    let mut vertices = [EchoPathVertex {
        total_path_m: 0.0,
        excess_path_m: 0.0,
        final_interaction_city_enu_m: [0.0; 3],
        arrival_vector_city_enu_m: [0.0; 3],
        material_pressure: [0.0; 3],
        predicted_received_pressure: [0.0; 3],
    }; 3];
    for vertex in &mut vertices {
        *vertex = EchoPathVertex {
            total_path_m: reader.f32()?,
            excess_path_m: reader.f32()?,
            final_interaction_city_enu_m: reader.vec3()?,
            arrival_vector_city_enu_m: reader.vec3()?,
            material_pressure: reader.vec3()?,
            predicted_received_pressure: reader.vec3()?,
        };
    }
    Ok(EchoPathRecord {
        path_key,
        geometry_key,
        material_key,
        kind,
        vertices,
    })
}

const fn encoded_path_bytes() -> usize {
    52 + 3 * 56
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], EchoAuthorityError> {
        if self.remaining.len() < count {
            return malformed("unexpected end of binary table");
        }
        let (head, tail) = self.remaining.split_at(count);
        self.remaining = tail;
        Ok(head)
    }

    fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    fn u8(&mut self) -> Result<u8, EchoAuthorityError> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, EchoAuthorityError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => malformed("boolean field is not zero or one"),
        }
    }

    fn u16(&mut self) -> Result<u16, EchoAuthorityError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("length checked"),
        ))
    }

    fn u32(&mut self) -> Result<u32, EchoAuthorityError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("length checked"),
        ))
    }

    fn f32(&mut self) -> Result<f32, EchoAuthorityError> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn vec3(&mut self) -> Result<[f32; 3], EchoAuthorityError> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }

    fn digest(&mut self) -> Result<Sha256Digest, EchoAuthorityError> {
        Ok(Sha256Digest(
            self.take(32)?.try_into().expect("length checked"),
        ))
    }

    fn spatial_key(&mut self) -> Result<StableSpatialKey, EchoAuthorityError> {
        Ok(StableSpatialKey(
            self.take(16)?.try_into().expect("length checked"),
        ))
    }

    fn geometry_key(&mut self) -> Result<StableGeometryKey, EchoAuthorityError> {
        Ok(StableGeometryKey(
            self.take(16)?.try_into().expect("length checked"),
        ))
    }

    fn path_key(&mut self) -> Result<StablePathKey, EchoAuthorityError> {
        Ok(StablePathKey(
            self.take(16)?.try_into().expect("length checked"),
        ))
    }

    fn count(
        &mut self,
        label: &str,
        minimum_record_bytes: usize,
    ) -> Result<usize, EchoAuthorityError> {
        let count = self.u32()? as usize;
        if count > self.remaining.len() / minimum_record_bytes {
            return malformed(format!("{label} count exceeds remaining table bytes"));
        }
        Ok(count)
    }
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_f32(out: &mut Vec<u8>, value: f32) {
    write_u32(out, value.to_bits());
}

fn write_vec3(out: &mut Vec<u8>, value: [f32; 3]) {
    for component in value {
        write_f32(out, component);
    }
}

fn finite3(value: [f32; 3]) -> bool {
    value.iter().all(|component| component.is_finite())
}

fn subtract(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|axis| left[axis] - right[axis])
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn cross(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn length(vector: [f32; 3]) -> f32 {
    dot(vector, vector).sqrt()
}

fn normalize(vector: [f32; 3]) -> [f32; 3] {
    let magnitude = length(vector);
    vector.map(|component| component / magnitude)
}

fn distance(left: [f32; 3], right: [f32; 3]) -> f32 {
    length(subtract(left, right))
}

fn triangle_area_twice(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f32 {
    length(cross(subtract(b, a), subtract(c, a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_query(table: &EchoAuthorityTable) -> EchoAuthorityQuery {
        EchoAuthorityQuery {
            anchor: table.anchors()[0].key,
            source_position_city_enu_m: table.anchors()[0].position_city_enu_m,
            listener_cell: table.listener_nodes()[0].listener_cell,
            listener_position_city_enu_m: [2.0, 2.0, 1.5],
        }
    }

    #[test]
    fn codec_is_canonical_and_declared_bindings_are_strict() {
        let table = EchoAuthorityFixture::facade(true).unwrap();
        let bytes = table.encode();
        assert_eq!(&bytes[..8], ECHO_AUTHORITY_MAGIC);
        let decoded = EchoAuthorityTable::decode(&bytes).unwrap();
        assert_eq!(decoded, table);
        assert_eq!(decoded.encode(), bytes);

        let declaration = EchoAuthorityDeclaration::for_table(&table);
        let loaded = EchoAuthorityTable::load_declared(
            Some(&bytes),
            &declaration,
            AuthorityAdmission::Mobile {
                retained_resident_bytes: 0,
            },
        )
        .unwrap();
        assert!(loaded.resident_bytes() < MOBILE_ECHO_AUTHORITY_CAP_BYTES);

        let mut stale = declaration.clone();
        stale.bindings.mesh_hash = Sha256Digest::from_bytes(b"deleted facade mesh");
        assert!(matches!(
            EchoAuthorityTable::load_declared(Some(&bytes), &stale, AuthorityAdmission::Desktop),
            Err(EchoAuthorityError::StaleBinding { field: "mesh hash" })
        ));
        assert!(matches!(
            EchoAuthorityTable::load_declared(None, &declaration, AuthorityAdmission::Desktop),
            Err(EchoAuthorityError::MissingDeclaredPayload)
        ));
        assert!(matches!(
            EchoAuthorityTable::load_declared(
                Some(&bytes),
                &declaration,
                AuthorityAdmission::Mobile {
                    retained_resident_bytes: MOBILE_ECHO_AUTHORITY_CAP_BYTES,
                },
            ),
            Err(EchoAuthorityError::MobileResidentCapExceeded { .. })
        ));
    }

    #[test]
    fn controlled_facade_and_deleted_facade_have_expected_plan() {
        let table = EchoAuthorityFixture::facade(true).unwrap();
        let plan = table.query(fixture_query(&table)).unwrap();
        assert_eq!(plan.telemetry, EchoQueryTelemetry::Ready);
        assert_eq!(plan.taps.len(), 1);
        assert_eq!(plan.taps[0].kind, EchoPathKind::Specular);
        assert!((length(plan.taps[0].arrival_direction_city_enu) - 1.0).abs() < 1.0e-5);

        let deleted = EchoAuthorityFixture::facade(false).unwrap();
        let plan = deleted.query(fixture_query(&deleted)).unwrap();
        assert_eq!(plan.telemetry, EchoQueryTelemetry::ZeroValidCandidates);
        assert!(plan.taps.is_empty());
    }

    #[test]
    fn corner_is_reserved_and_anchor_tolerance_is_silent_telemetry() {
        let table = EchoAuthorityFixture::corner(true).unwrap();
        let mut query = fixture_query(&table);
        let plan = table.query(query).unwrap();
        assert_eq!(plan.taps.len(), 1);
        assert_eq!(plan.taps[0].kind, EchoPathKind::Diffraction);

        query.source_position_city_enu_m[0] += 0.021;
        let plan = table.query(query).unwrap();
        assert_eq!(plan.telemetry, EchoQueryTelemetry::AnchorOutOfDomain);

        let deleted = EchoAuthorityFixture::corner(false).unwrap();
        let plan = deleted.query(fixture_query(&deleted)).unwrap();
        assert_eq!(plan.telemetry, EchoQueryTelemetry::ZeroValidCandidates);
    }

    #[test]
    fn ranking_keeps_three_specular_paths_and_the_nlos_diffraction_reservation() {
        let mut table = EchoAuthorityFixture::corner(true).unwrap();
        let PlanTileContent::Paths(paths) = &mut table.tiles[0].content else {
            unreachable!()
        };
        let diffraction = paths[0].clone();
        for rank in 0..4 {
            let mut specular = diffraction.clone();
            specular.kind = EchoPathKind::Specular;
            specular.path_key = StablePathKey::derive("echo-path", &[b's', rank]);
            for vertex in &mut specular.vertices {
                vertex.excess_path_m = 120.0;
                vertex.predicted_received_pressure = [
                    0.010 - f32::from(rank) * 0.001,
                    0.009 - f32::from(rank) * 0.001,
                    0.008 - f32::from(rank) * 0.001,
                ];
            }
            paths.push(specular);
        }
        paths.sort_by_key(|path| path.path_key);
        table.validate().unwrap();

        let first = table.query(fixture_query(&table)).unwrap();
        let second = table.query(fixture_query(&table)).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.taps.len(), 4);
        assert_eq!(
            first
                .taps
                .iter()
                .filter(|tap| tap.kind == EchoPathKind::Specular)
                .count(),
            3
        );
        assert!(
            first
                .taps
                .iter()
                .any(|tap| tap.kind == EchoPathKind::Diffraction)
        );
    }

    #[test]
    fn explicit_no_plan_boundary_dominates_overlap() {
        let mut table = EchoAuthorityFixture::facade(true).unwrap();
        let base = table.tiles[0].clone();
        table.tiles.push(PlanTile {
            key: StableSpatialKey::derive("plan-tile", b"explicit-boundary"),
            content: PlanTileContent::NoPlan(ExplicitNoPlanReason::VisibilityDiscontinuity),
            ..base
        });
        table.tiles.sort_by_key(|tile| tile.key);
        table.validate().unwrap();
        let plan = table.query(fixture_query(&table)).unwrap();
        assert_eq!(
            plan.telemetry,
            EchoQueryTelemetry::ExplicitNoPlan(ExplicitNoPlanReason::VisibilityDiscontinuity)
        );
    }
}
