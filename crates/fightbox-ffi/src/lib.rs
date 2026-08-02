//! Defensive C ABI for embedding the retained Fightbox Steam Audio graph.
//!
//! A session has exactly one control-thread owner and one audio-thread owner.
//! Listener/source updates and telemetry queries must be serialized on the
//! control thread. `fb_session_render_block` may run concurrently on one audio
//! thread. Destruction requires both roles to be stopped and joined.

#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    cell::UnsafeCell,
    ffi::{CStr, c_char},
    mem::{align_of, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    ptr, slice,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use fightbox_api::{EnuVector3, ListenerState, Pose, ReferenceLevel};
use fightbox_runtime::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES,
    PropagationRenderBlock, SimulationRunner, SimulationUpdate, SourceMotion,
};
use fightbox_runtime::{SnapshotPublication, SnapshotReader, SnapshotWriter};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, BakedProbeBatch, GovernorTransitionReason, MemoryTrackingStatus,
    MultiSourceDescriptor, PROBE_BATCH_METADATA_SCHEMA, PathQualityLevel, ProbeBatchMetadata,
    QualityGovernorTelemetry, QualityTier, ReflectionQualityLevel, ReverbStrategy,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SourceQualityLevel, SteamAudioRenderGraph,
    SteamAudioSimulationRunner, build_multi_source_session_for_tier,
};
use serde::Deserialize;

/// Stable status returned by every fallible FFI operation.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbResult {
    FbOk = 0,
    FbInvalidArgument = 1,
    FbInvalidState = 2,
    FbIoError = 3,
    FbInvalidPackage = 4,
    FbInvalidBake = 5,
    FbBackendUnavailable = 6,
    FbBackendError = 7,
    FbBufferTooSmall = 8,
    FbPanic = 9,
}

/// Named construction-time quality tier.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbQualityTier {
    FbQualityDesktop = 0,
    FbQualityMobile = 1,
}

/// Three-component vector in right-handed local ENU coordinates.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbVec3 {
    pub east_m: f32,
    pub north_m: f32,
    pub up_m: f32,
}

/// Position and orientation in right-handed local ENU coordinates.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbPose {
    pub position: FbVec3,
    pub forward: FbVec3,
    pub up: FbVec3,
}

/// Immutable session construction settings.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSessionConfig {
    pub sample_rate_hz: u32,
    pub block_size_frames: u32,
    pub source_count: u32,
    /// Relative source level used by the quality governor.
    pub default_source_level_db: f32,
    /// One of `FbQualityTier`. Zero selects Desktop, preserving legacy
    /// zero-initialized caller behavior. Unknown values are rejected.
    pub quality_tier: u32,
}

impl Default for FbSessionConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            block_size_frames: 512,
            source_count: 1,
            default_source_level_db: 0.0,
            quality_tier: FbQualityTier::FbQualityDesktop as u32,
        }
    }
}

/// Complete control-thread update for one stable source index.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSourceUpdate {
    /// Zero means inactive; any other value means active.
    pub active: u8,
    pub pose: FbPose,
    pub linear_velocity_mps: FbVec3,
}

/// Opaque retained session. Its layout is intentionally unavailable to C.
pub struct FbSession {
    _private: [u8; 0],
}

struct ControlState {
    runner: SteamAudioSimulationRunner,
    update: SimulationUpdate,
    update_sequence: u64,
    orientation_writer: SnapshotWriter<ListenerOrientation>,
    active_sources_writer: SnapshotWriter<[bool; MAX_ACTIVE_SOURCES]>,
}

struct RenderState {
    graph: SteamAudioRenderGraph,
    left: Vec<f32>,
    right: Vec<f32>,
    orientation_reader: SnapshotReader<ListenerOrientation>,
    active_sources_reader: SnapshotReader<[bool; MAX_ACTIVE_SOURCES]>,
}

struct SessionInner {
    control: UnsafeCell<Option<ControlState>>,
    render: UnsafeCell<Option<RenderState>>,
    source_count: usize,
    block_size: usize,
    last_render_ns: AtomicU64,
    ffi_render_buffers_bytes: u64,
}

// Safety: the public contract assigns `control` and `render` to distinct,
// serialized threads. Cross-role state uses bounded snapshot publications or
// atomics, and the backend supports concurrent graph reads by construction.
unsafe impl Send for SessionInner {}
unsafe impl Sync for SessionInner {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeMetadataWire {
    schema_version: String,
    steam_audio_version: String,
    upstream_commit: String,
    probe_count: u32,
    path_data_size_bytes: u64,
    serialized_size_bytes: u64,
    content_sha256: String,
    bake_progress_callback_count: u32,
    final_bake_progress_millionths: u32,
}

/// Creates a retained session.
///
/// Thread safety: call on the control thread before starting audio. `config`,
/// both UTF-8 NUL-terminated paths, and `out_session` are borrowed only for
/// this call. On failure, `*out_session` is null. The package path names a
/// `.fightbox` directory; the bake path names a directory containing
/// `probe-batch.bin`, `probe-batch-metadata.json`, and
/// `city-bake-manifest.json`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_create(
    config: *const FbSessionConfig,
    package_path_utf8: *const c_char,
    bake_path_utf8: *const c_char,
    out_session: *mut *mut FbSession,
) -> FbResult {
    ffi_boundary(|| {
        if !valid_mut_ptr(out_session) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: `out_session` was checked for null and alignment.
        unsafe { out_session.write(ptr::null_mut()) };
        if !valid_const_ptr(config) || package_path_utf8.is_null() || bake_path_utf8.is_null() {
            return FbResult::FbInvalidArgument;
        }
        // Safety: pointers are valid for this call under the C API contract.
        let config = unsafe { *config };
        let package = match unsafe { path_from_c(package_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };
        let bake = match unsafe { path_from_c(bake_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };
        let session = match SessionInner::create(config, &package, &bake) {
            Ok(session) => session,
            Err(result) => return result,
        };
        let raw = Box::into_raw(Box::new(session)).cast::<FbSession>();
        // Safety: `out_session` was validated and is uniquely borrowed by this call.
        unsafe { out_session.write(raw) };
        FbResult::FbOk
    })
}

/// Publishes a listener pose and advances the control-side simulation cadence.
///
/// Thread safety: control thread only. Calls must not overlap other update or
/// telemetry calls. This may run Steam Audio simulation and must never be
/// called from the audio callback. It may run concurrently with render.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_update_listener(
    session: *mut FbSession,
    pose: *const FbPose,
    linear_velocity_mps: *const FbVec3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if !valid_const_ptr(pose) || !valid_const_ptr(linear_velocity_mps) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: both input pointers were checked and are borrowed for this call.
        let pose = match pose_from_ffi(unsafe { *pose }) {
            Some(value) => value,
            None => return FbResult::FbInvalidArgument,
        };
        // Safety: pointer was checked above.
        let velocity = match vector_from_ffi(unsafe { *linear_velocity_mps }) {
            Some(value) => value,
            None => return FbResult::FbInvalidArgument,
        };
        // Safety: the API requires serialized control-thread access.
        let Some(control) = (unsafe { &mut *session.control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        control.update.listener = ListenerState {
            pose,
            linear_velocity_mps: velocity,
        };
        control.orientation_writer.publish(ListenerOrientation {
            forward: pose.forward,
            up: pose.up,
        });
        advance_simulation(session, control)
    })
}

/// Publishes one source's motion and advances the control-side simulation cadence.
///
/// Thread safety: control thread only, serialized with listener updates and
/// telemetry. `source_index` is stable and zero-based.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_update_source(
    session: *mut FbSession,
    source_index: u32,
    update: *const FbSourceUpdate,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let Ok(index) = usize::try_from(source_index) else {
            return FbResult::FbInvalidArgument;
        };
        if index >= session.source_count || !valid_const_ptr(update) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: `update` was checked and is borrowed for this call.
        let update = unsafe { *update };
        let Some(pose) = pose_from_ffi(update.pose) else {
            return FbResult::FbInvalidArgument;
        };
        let Some(velocity) = vector_from_ffi(update.linear_velocity_mps) else {
            return FbResult::FbInvalidArgument;
        };
        // Safety: the API requires serialized control-thread access.
        let Some(control) = (unsafe { &mut *session.control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let active = update.active != 0;
        control.update.sources[index] = SourceMotion {
            active,
            pose,
            linear_velocity_mps: velocity,
        };
        control
            .active_sources_writer
            .publish(std::array::from_fn(|source_index| {
                control.update.sources[source_index].active
            }));
        advance_simulation(session, control)
    })
}

/// Renders exactly one configured block.
///
/// `source_mono` contains `source_count * block_size_frames` finite samples in
/// source-major order. `out_interleaved_stereo` contains
/// `block_size_frames * 2` writable samples and must not overlap the input.
///
/// Thread safety: one audio thread only. The function is allocation-free and
/// lock-free after construction. It may run concurrently with control updates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_render_block(
    session: *mut FbSession,
    source_mono: *const f32,
    source_sample_count: usize,
    out_interleaved_stereo: *mut f32,
    out_sample_count: usize,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let expected_input = match session.source_count.checked_mul(session.block_size) {
            Some(value) => value,
            None => return FbResult::FbInvalidState,
        };
        let expected_output = match session.block_size.checked_mul(2) {
            Some(value) => value,
            None => return FbResult::FbInvalidState,
        };
        if source_sample_count != expected_input
            || out_sample_count != expected_output
            || !valid_slice_ptr(source_mono, source_sample_count)
            || !valid_mut_slice_ptr(out_interleaved_stereo, out_sample_count)
            || ranges_overlap(
                source_mono.cast::<u8>(),
                source_sample_count.saturating_mul(size_of::<f32>()),
                out_interleaved_stereo.cast::<u8>(),
                out_sample_count.saturating_mul(size_of::<f32>()),
            )
        {
            return FbResult::FbInvalidArgument;
        }
        // Safety: pointer, length, alignment, and non-overlap were validated.
        let input = unsafe { slice::from_raw_parts(source_mono, source_sample_count) };
        // Safety: pointer, length, alignment, and non-overlap were validated.
        let output = unsafe { slice::from_raw_parts_mut(out_interleaved_stereo, out_sample_count) };
        if input.iter().any(|sample| !sample.is_finite()) {
            output.fill(0.0);
            return FbResult::FbInvalidArgument;
        }
        // Safety: the API requires serialized audio-thread access.
        let Some(render) = (unsafe { &mut *session.render.get() }).as_mut() else {
            output.fill(0.0);
            return FbResult::FbInvalidState;
        };

        render.left.fill(0.0);
        render.right.fill(0.0);
        let empty = &[];
        let mut sources = [BackendSourceBlock {
            source_index: 0,
            input_mono: empty,
        }; MAX_ACTIVE_SOURCES];
        let mut active_count = 0;
        let active_sources = render.active_sources_reader.read();
        for index in 0..session.source_count {
            if active_sources[index] {
                let start = index * session.block_size;
                sources[active_count] = BackendSourceBlock {
                    source_index: index,
                    input_mono: &input[start..start + session.block_size],
                };
                active_count += 1;
            }
        }
        let started = Instant::now();
        let result = render.graph.render_block(PropagationRenderBlock {
            listener_orientation: render.orientation_reader.read(),
            sources: &sources[..active_count],
            output_left: &mut render.left,
            output_right: &mut render.right,
        });
        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        session
            .last_render_ns
            .store(elapsed.max(1), Ordering::Release);
        if result.is_err()
            || render
                .left
                .iter()
                .chain(&render.right)
                .any(|sample| !sample.is_finite())
        {
            output.fill(0.0);
            return FbResult::FbBackendError;
        }
        for (frame, pair) in output.chunks_exact_mut(2).enumerate() {
            pair[0] = render.left[frame];
            pair[1] = render.right[frame];
        }
        FbResult::FbOk
    })
}

/// Copies a NUL-terminated delivered-quality/timing JSON snapshot.
///
/// Thread safety: control thread only, serialized with updates. `out_required`
/// always receives the required byte count including the NUL terminator.
/// Passing a null buffer with zero capacity is the supported size query.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_telemetry_json(
    session: *mut FbSession,
    buffer: *mut c_char,
    buffer_capacity: usize,
    out_required: *mut usize,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if !valid_mut_ptr(out_required)
            || (buffer_capacity != 0 && !valid_mut_slice_ptr(buffer, buffer_capacity))
            || (buffer_capacity == 0 && !buffer.is_null())
        {
            return FbResult::FbInvalidArgument;
        }
        // Safety: the API requires serialized control-thread access.
        let Some(control) = (unsafe { &mut *session.control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        observe_latest_render_timing(session, control);
        let json = telemetry_json(
            control.runner.quality_governor_telemetry(),
            session.ffi_render_buffers_bytes,
        );
        let required = match json.len().checked_add(1) {
            Some(value) => value,
            None => return FbResult::FbInvalidState,
        };
        // Safety: `out_required` was validated above.
        unsafe { out_required.write(required) };
        if buffer_capacity < required {
            return FbResult::FbBufferTooSmall;
        }
        // Safety: capacity is sufficient and buffer was validated.
        unsafe {
            ptr::copy_nonoverlapping(json.as_ptr(), buffer.cast::<u8>(), json.len());
            buffer.add(json.len()).write(0);
        }
        FbResult::FbOk
    })
}

/// Destroys a session and releases all Rust and Steam Audio resources.
///
/// Thread safety: call only after the control and audio threads are stopped
/// and joined. A handle must be destroyed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_destroy(session: *mut FbSession) -> FbResult {
    ffi_boundary(|| {
        if !valid_session_ptr(session) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: the caller transfers the unique allocation back exactly once.
        drop(unsafe { Box::from_raw(session.cast::<SessionInner>()) });
        FbResult::FbOk
    })
}

impl SessionInner {
    fn create(
        config: FbSessionConfig,
        package_path: &Path,
        bake_path: &Path,
    ) -> Result<Self, FbResult> {
        let source_count =
            usize::try_from(config.source_count).map_err(|_| FbResult::FbInvalidArgument)?;
        let block_size =
            usize::try_from(config.block_size_frames).map_err(|_| FbResult::FbInvalidArgument)?;
        let quality_tier =
            quality_tier_from_ffi(config.quality_tier).ok_or(FbResult::FbInvalidArgument)?;
        if config.sample_rate_hz == 0
            || block_size == 0
            || source_count == 0
            || source_count > quality_tier.active_source_cap()
            || !config.default_source_level_db.is_finite()
            || i32::try_from(config.sample_rate_hz).is_err()
            || i32::try_from(config.block_size_frames).is_err()
        {
            return Err(FbResult::FbInvalidArgument);
        }
        let loaded =
            fightbox_world::read_package(package_path).map_err(|_| FbResult::FbInvalidPackage)?;
        let mesh = scene_mesh(&loaded)?;
        let baked = load_bake(bake_path)?;
        verify_bake_identity(&loaded, bake_path, &baked)?;
        let descriptors = (0..source_count)
            .map(|_| {
                MultiSourceDescriptor::at(EnuVector3::default()).with_reference_level(
                    ReferenceLevel::CreativeDb {
                        db: config.default_source_level_db,
                    },
                )
            })
            .collect::<Vec<_>>();
        let audio = AudioConfig {
            sample_rate_hz: config.sample_rate_hz as i32,
            frame_size: config.block_size_frames as i32,
        };
        let simulation = quality_tier.simulation_defaults();
        let (mut runner, graph) = build_multi_source_session_for_tier(
            &mesh,
            &baked,
            audio,
            simulation,
            &descriptors,
            quality_tier,
        )
        .map_err(|error| {
            if matches!(error, fightbox_steam_audio::BackendError::SdkUnavailable(_)) {
                FbResult::FbBackendUnavailable
            } else {
                FbResult::FbBackendError
            }
        })?;
        let update = default_simulation_update();
        runner.update_inputs(&update);
        runner
            .run_direct()
            .and_then(|_| runner.run_pathing())
            .and_then(|_| runner.run_reflections())
            .map_err(|_| FbResult::FbBackendError)?;
        let listener = update.listener.pose;
        let (orientation_writer, orientation_reader) =
            SnapshotPublication::new(ListenerOrientation {
                forward: listener.forward,
                up: listener.up,
            });
        let (active_sources_writer, active_sources_reader) =
            SnapshotPublication::new([false; MAX_ACTIVE_SOURCES]);
        Ok(Self {
            control: UnsafeCell::new(Some(ControlState {
                runner,
                update,
                update_sequence: 0,
                orientation_writer,
                active_sources_writer,
            })),
            render: UnsafeCell::new(Some(RenderState {
                graph,
                left: vec![0.0; block_size],
                right: vec![0.0; block_size],
                orientation_reader,
                active_sources_reader,
            })),
            source_count,
            block_size,
            last_render_ns: AtomicU64::new(0),
            ffi_render_buffers_bytes: (block_size as u64)
                .saturating_mul(2)
                .saturating_mul(size_of::<f32>() as u64),
        })
    }
}

fn default_simulation_update() -> SimulationUpdate {
    SimulationUpdate {
        listener: ListenerState {
            pose: Pose {
                position: EnuVector3::default(),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            linear_velocity_mps: EnuVector3::default(),
        },
        sources: [SourceMotion::default(); MAX_ACTIVE_SOURCES],
    }
}

fn advance_simulation(session: &SessionInner, control: &mut ControlState) -> FbResult {
    observe_latest_render_timing(session, control);
    control.runner.update_inputs(&control.update);
    if control.runner.run_direct().is_err() {
        return FbResult::FbBackendError;
    }
    control.update_sequence = control.update_sequence.wrapping_add(1);
    if control.update_sequence.is_multiple_of(4) && control.runner.run_pathing().is_err() {
        return FbResult::FbBackendError;
    }
    if control.update_sequence.is_multiple_of(12) && control.runner.run_reflections().is_err() {
        return FbResult::FbBackendError;
    }
    FbResult::FbOk
}

fn observe_latest_render_timing(session: &SessionInner, control: &mut ControlState) {
    let elapsed = session.last_render_ns.swap(0, Ordering::AcqRel);
    if elapsed != 0 {
        control.runner.observe_render_timing(elapsed);
    }
}

fn scene_mesh(
    loaded: &fightbox_world::LoadedPackage,
) -> Result<fightbox_steam_audio::SceneMesh, FbResult> {
    let triangles = loaded
        .mesh
        .triangles
        .iter()
        .map(|triangle| {
            Ok([
                i32::try_from(triangle[0]).map_err(|_| FbResult::FbInvalidPackage)?,
                i32::try_from(triangle[1]).map_err(|_| FbResult::FbInvalidPackage)?,
                i32::try_from(triangle[2]).map_err(|_| FbResult::FbInvalidPackage)?,
            ])
        })
        .collect::<Result<Vec<_>, FbResult>>()?;
    let material_indices = loaded
        .mesh
        .material_ids
        .iter()
        .map(|index| i32::try_from(*index).map_err(|_| FbResult::FbInvalidPackage))
        .collect::<Result<Vec<_>, _>>()?;
    let materials = loaded
        .materials
        .iter()
        .map(|(_, material)| AcousticMaterial {
            absorption: material.absorption,
            scattering: material.scattering,
            transmission: material.transmission,
        })
        .collect();
    Ok(fightbox_steam_audio::SceneMesh {
        vertices_enu_m: loaded
            .mesh
            .vertices_enu_m
            .iter()
            .map(|vertex| {
                fightbox_steam_audio::EnuVector3::new(vertex.east_m, vertex.north_m, vertex.up_m)
            })
            .collect(),
        triangles,
        material_indices,
        materials,
    })
}

fn load_bake(path: &Path) -> Result<BakedProbeBatch, FbResult> {
    let bytes = std::fs::read(path.join("probe-batch.bin")).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FbResult::FbInvalidBake
        } else {
            FbResult::FbIoError
        }
    })?;
    let metadata_text = std::fs::read_to_string(path.join("probe-batch-metadata.json"))
        .map_err(|_| FbResult::FbInvalidBake)?;
    let wire: ProbeMetadataWire =
        serde_json::from_str(&metadata_text).map_err(|_| FbResult::FbInvalidBake)?;
    if wire.schema_version != PROBE_BATCH_METADATA_SCHEMA
        || wire.steam_audio_version != STEAM_AUDIO_VERSION
        || wire.upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT
    {
        return Err(FbResult::FbInvalidBake);
    }
    let baked = BakedProbeBatch {
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: wire.probe_count,
            path_data_size_bytes: wire.path_data_size_bytes,
            serialized_size_bytes: wire.serialized_size_bytes,
            content_sha256: wire.content_sha256,
            bake_progress_callback_count: wire.bake_progress_callback_count,
            final_bake_progress_millionths: wire.final_bake_progress_millionths,
        },
        bytes,
    };
    baked.validate().map_err(|_| FbResult::FbInvalidBake)?;
    Ok(baked)
}

fn verify_bake_identity(
    loaded: &fightbox_world::LoadedPackage,
    bake_path: &Path,
    baked: &BakedProbeBatch,
) -> Result<(), FbResult> {
    let bytes = std::fs::read(bake_path.join("city-bake-manifest.json"))
        .map_err(|_| FbResult::FbInvalidBake)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| FbResult::FbInvalidBake)?;
    for (field, expected) in [
        (
            "mesh_content_sha256",
            loaded.manifest.mesh_content_sha256.as_str(),
        ),
        (
            "materials_content_sha256",
            loaded.manifest.materials_content_sha256.as_str(),
        ),
        ("probe_batch_sha256", baked.metadata.content_sha256.as_str()),
    ] {
        if value.get(field).and_then(serde_json::Value::as_str) != Some(expected) {
            return Err(FbResult::FbInvalidBake);
        }
    }
    Ok(())
}

fn telemetry_json(
    telemetry: Option<QualityGovernorTelemetry>,
    ffi_render_buffers_bytes: u64,
) -> String {
    let Some(value) = telemetry else {
        return String::from(r#"{"available":false}"#);
    };
    let mut sources = String::new();
    for (index, source) in value.sources[..usize::from(value.source_count)]
        .iter()
        .enumerate()
    {
        if index != 0 {
            sources.push(',');
        }
        sources.push_str(&format!(
            concat!(
                r#"{{"source_index":{},"quality":"{}","predicted_audibility_db":{},"#,
                r#""physically_calibrated":{},"below_hearing_threshold":{},"#,
                r#""transport_advances":{}}}"#
            ),
            source.source_index,
            source_quality_name(source.quality),
            source.predicted_audibility_db,
            source.physically_calibrated,
            source.below_hearing_threshold,
            source.transport_advances,
        ));
    }
    let tracked_at_create_bytes = value
        .memory
        .tracked_at_create_bytes
        .saturating_add(ffi_render_buffers_bytes);
    let tracked_current_bytes = value
        .memory
        .tracked_current_bytes
        .saturating_add(ffi_render_buffers_bytes);
    let tracked_peak_bytes = value
        .memory
        .tracked_peak_bytes
        .saturating_add(ffi_render_buffers_bytes);
    format!(
        concat!(
            r#"{{"available":true,"quality_tier":"{}","tier_source_cap":{},"sequence":{},"ladder_position":{},"reason":"{}","#,
            r#""timing_ns":{{"p50":{},"p95":{},"p99":{},"p99_9":{},"deadline_misses":{}}},"#,
            r#""simulation_lateness_ns":[{},{},{}],"delivered_quality":{{"#,
            r#""reflections":{{"level":"{}","rays":{},"diffuse_samples":{},"bounces":{},"#,
            r#""ir_duration_s":{},"cadence_divisor":{}}},"pathing":"{}","#,
            r#""ambisonic_order":{},"reverb":"{}","reflection_output_gain":{},"sources":[{}]}},"#,
            r#""memory":{{"scope":"configuration_known_payloads_and_capacities_not_process_total","#,
            r#""tracked_at_create_bytes":{},"tracked_current_bytes":{},"tracked_peak_bytes":{},"categories":{{"#,
            r#""snapshot_ring_payload_bytes":{},"reflection_ir_payload_capacity_bytes":{},"#,
            r#""audio_buffer_payload_bytes":{},"engine_render_scratch_bytes":{},"#,
            r#""ffi_render_buffers_bytes":{},"propagation_delay_line_bytes":{},"retained_bake_bytes":{}}},"#,
            r#""untracked":[{{"category":"steam_audio_sdk_internal","status":"{}","#,
            r#""includes":"allocator_overhead,effect_workspaces,hrtf,scene,probe_and_simulator_storage"}}]}}}}"#
        ),
        quality_tier_name(value.quality_tier),
        value.tier_source_cap,
        value.sequence,
        value.ladder_position,
        transition_reason_name(value.reason),
        value.p50_ns,
        value.p95_ns,
        value.p99_ns,
        value.p99_9_ns,
        value.callback_deadline_misses,
        value.simulation_lateness_ns[0],
        value.simulation_lateness_ns[1],
        value.simulation_lateness_ns[2],
        reflection_quality_name(value.reflections.level),
        value.reflections.rays,
        value.reflections.diffuse_samples,
        value.reflections.bounces,
        value.reflections.ir_duration_s,
        value.reflections.cadence_divisor,
        path_quality_name(value.pathing),
        value.ambisonic_order,
        reverb_name(value.reverb),
        value.reflection_output_gain,
        sources,
        tracked_at_create_bytes,
        tracked_current_bytes,
        tracked_peak_bytes,
        value.memory.snapshot_ring_payload_bytes,
        value.memory.reflection_ir_payload_capacity_bytes,
        value.memory.audio_buffer_payload_bytes,
        value.memory.render_scratch_bytes,
        ffi_render_buffers_bytes,
        value.memory.propagation_delay_line_bytes,
        value.memory.retained_bake_bytes,
        memory_tracking_status_name(value.memory.steam_audio_sdk_internal),
    )
}

fn quality_tier_from_ffi(value: u32) -> Option<QualityTier> {
    match value {
        value if value == FbQualityTier::FbQualityDesktop as u32 => Some(QualityTier::Desktop),
        value if value == FbQualityTier::FbQualityMobile as u32 => Some(QualityTier::Mobile),
        _ => None,
    }
}

fn quality_tier_name(value: QualityTier) -> &'static str {
    match value {
        QualityTier::Desktop => "desktop",
        QualityTier::Mobile => "mobile",
    }
}

fn memory_tracking_status_name(value: MemoryTrackingStatus) -> &'static str {
    match value {
        MemoryTrackingStatus::Tracked => "tracked",
        MemoryTrackingStatus::Untracked => "untracked",
    }
}

fn transition_reason_name(value: GovernorTransitionReason) -> &'static str {
    match value {
        GovernorTransitionReason::Initial => "initial",
        GovernorTransitionReason::RenderP99OverBudget => "render_p99_over_budget",
        GovernorTransitionReason::RenderP999OverCeiling => "render_p99_9_over_ceiling",
        GovernorTransitionReason::RenderDeadlineMiss => "render_deadline_miss",
        GovernorTransitionReason::SimulationLate => "simulation_late",
        GovernorTransitionReason::SustainedHeadroom => "sustained_headroom",
        GovernorTransitionReason::AtMinimumQuality => "at_minimum_quality",
        GovernorTransitionReason::AtFullQuality => "at_full_quality",
    }
}

fn reflection_quality_name(value: ReflectionQualityLevel) -> &'static str {
    match value {
        ReflectionQualityLevel::Full => "full",
        ReflectionQualityLevel::Reduced => "reduced",
        ReflectionQualityLevel::Minimum => "minimum",
    }
}

fn path_quality_name(value: PathQualityLevel) -> &'static str {
    match value {
        PathQualityLevel::Full => "full",
        PathQualityLevel::NoValidation => "no_validation",
        PathQualityLevel::PrimaryOnly => "primary_only",
    }
}

fn reverb_name(value: ReverbStrategy) -> &'static str {
    match value {
        ReverbStrategy::SdkMixerConvolution => "sdk_mixer_convolution",
        ReverbStrategy::Hybrid => "hybrid",
        ReverbStrategy::Baked => "baked",
        ReverbStrategy::ListenerCentric => "listener_centric",
        ReverbStrategy::ShortIrLowerOrder => "short_ir_lower_order",
    }
}

fn source_quality_name(value: SourceQualityLevel) -> &'static str {
    match value {
        SourceQualityLevel::Full => "full",
        SourceQualityLevel::DirectOnly => "direct_only",
        SourceQualityLevel::Virtualized => "virtualized",
    }
}

fn vector_from_ffi(value: FbVec3) -> Option<EnuVector3> {
    let value = EnuVector3::new(value.east_m, value.north_m, value.up_m);
    value.is_finite().then_some(value)
}

fn pose_from_ffi(value: FbPose) -> Option<Pose> {
    let pose = Pose {
        position: vector_from_ffi(value.position)?,
        forward: vector_from_ffi(value.forward)?,
        up: vector_from_ffi(value.up)?,
    };
    let forward_length = length_squared(pose.forward);
    let up_length = length_squared(pose.up);
    let cross_length = length_squared(cross(pose.forward, pose.up));
    (forward_length > 1.0e-8 && up_length > 1.0e-8 && cross_length > 1.0e-8).then_some(pose)
}

fn length_squared(value: EnuVector3) -> f32 {
    value.east_m * value.east_m + value.north_m * value.north_m + value.up_m * value.up_m
}

fn cross(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.north_m * right.up_m - left.up_m * right.north_m,
        left.up_m * right.east_m - left.east_m * right.up_m,
        left.east_m * right.north_m - left.north_m * right.east_m,
    )
}

fn ffi_boundary(operation: impl FnOnce() -> FbResult) -> FbResult {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(FbResult::FbPanic)
}

unsafe fn path_from_c(pointer: *const c_char) -> Result<PathBuf, FbResult> {
    // Safety: the caller guarantees a readable NUL-terminated C string.
    let bytes = unsafe { CStr::from_ptr(pointer) };
    let text = bytes.to_str().map_err(|_| FbResult::FbInvalidArgument)?;
    if text.is_empty() {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(PathBuf::from(text))
}

unsafe fn session_ref<'a>(session: *mut FbSession) -> Option<&'a SessionInner> {
    if !valid_session_ptr(session) {
        return None;
    }
    // Safety: caller owns a live handle for the duration of the operation.
    Some(unsafe { &*session.cast::<SessionInner>() })
}

fn valid_session_ptr(pointer: *mut FbSession) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(align_of::<SessionInner>())
}

fn valid_const_ptr<T>(pointer: *const T) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(align_of::<T>())
}

fn valid_mut_ptr<T>(pointer: *mut T) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(align_of::<T>())
}

fn valid_slice_ptr<T>(pointer: *const T, length: usize) -> bool {
    length == 0 || valid_const_ptr(pointer)
}

fn valid_mut_slice_ptr<T>(pointer: *mut T, length: usize) -> bool {
    length == 0 || valid_mut_ptr(pointer)
}

fn ranges_overlap(left: *const u8, left_len: usize, right: *const u8, right_len: usize) -> bool {
    let left_start = left.addr();
    let right_start = right.addr();
    let Some(left_end) = left_start.checked_add(left_len) else {
        return true;
    };
    let Some(right_end) = right_start.checked_add(right_len) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_session() -> *mut FbSession {
        let session = SessionInner {
            control: UnsafeCell::new(None),
            render: UnsafeCell::new(None),
            source_count: 1,
            block_size: 4,
            last_render_ns: AtomicU64::new(0),
            ffi_render_buffers_bytes: 2 * 4 * size_of::<f32>() as u64,
        };
        Box::into_raw(Box::new(session)).cast()
    }

    #[test]
    fn null_and_misaligned_pointers_are_rejected() {
        // Safety: deliberately invalid pointers are rejected before dereference.
        unsafe {
            assert_eq!(
                fb_session_destroy(ptr::null_mut()),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_update_listener(ptr::null_mut(), ptr::null(), ptr::null()),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_destroy(1_usize as *mut FbSession),
                FbResult::FbInvalidArgument
            );
        }
    }

    #[test]
    fn handle_lifecycle_releases_a_live_allocation() {
        let session = fake_session();
        // Safety: this is a unique live allocation and is destroyed once.
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn render_validates_lengths_before_touching_backend_state() {
        let session = fake_session();
        let input = [0.0_f32; 4];
        let mut output = [1.0_f32; 8];
        // Safety: all buffers are valid for their declared lengths.
        let result =
            unsafe { fb_session_render_block(session, input.as_ptr(), 3, output.as_mut_ptr(), 8) };
        assert_eq!(result, FbResult::FbInvalidArgument);
        // Safety: this is a unique live allocation and is destroyed once.
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn telemetry_size_query_rejects_an_inactive_test_handle_without_writing_buffer() {
        let session = fake_session();
        let mut required = 0;
        // Safety: the output-size pointer is valid.
        let result =
            unsafe { fb_session_telemetry_json(session, ptr::null_mut(), 0, &mut required) };
        assert_eq!(result, FbResult::FbInvalidState);
        assert_eq!(required, 0);
        // Safety: this is a unique live allocation and is destroyed once.
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn ffi_quality_tier_values_round_trip_and_unknown_values_are_rejected() {
        for (wire, expected) in [
            (FbQualityTier::FbQualityDesktop as u32, QualityTier::Desktop),
            (FbQualityTier::FbQualityMobile as u32, QualityTier::Mobile),
        ] {
            assert_eq!(quality_tier_from_ffi(wire), Some(expected));
        }
        assert_eq!(quality_tier_from_ffi(2), None);
        assert_eq!(
            FbSessionConfig::default().quality_tier,
            FbQualityTier::FbQualityDesktop as u32
        );
        let mobile_defaults = quality_tier_from_ffi(FbQualityTier::FbQualityMobile as u32)
            .unwrap()
            .simulation_defaults();
        assert_eq!(mobile_defaults.reflection_rays, 512);
        assert_eq!(mobile_defaults.reflection_duration_s, 0.5);
        assert_eq!(mobile_defaults.reflection_order, 0);
        assert_eq!(mobile_defaults.pathing_order, 1);
    }

    #[test]
    fn create_rejects_an_invalid_tier_before_reading_package_paths() {
        let config = FbSessionConfig {
            quality_tier: u32::MAX,
            ..FbSessionConfig::default()
        };
        let path = std::ffi::CString::new("/definitely/not/a/fightbox/path").unwrap();
        let mut session: *mut FbSession = ptr::null_mut();
        // Safety: all pointers are valid for the duration of the call.
        let result = unsafe {
            fb_session_create(
                &config,
                path.as_ptr(),
                path.as_ptr(),
                &mut session as *mut *mut FbSession,
            )
        };
        assert_eq!(result, FbResult::FbInvalidArgument);
        assert!(session.is_null());
    }

    #[test]
    fn telemetry_json_reports_tracked_memory_without_claiming_sdk_internal_total() {
        let telemetry = QualityGovernorTelemetry {
            quality_tier: QualityTier::Mobile,
            tier_source_cap: 4,
            sequence: 1,
            ladder_position: 3,
            reason: GovernorTransitionReason::Initial,
            p50_ns: 10,
            p95_ns: 20,
            p99_ns: 30,
            p99_9_ns: 40,
            callback_deadline_misses: 0,
            simulation_lateness_ns: [0; 3],
            reflections: fightbox_steam_audio::DeliveredReflectionQuality {
                level: ReflectionQualityLevel::Reduced,
                rays: 512,
                diffuse_samples: 16,
                diffuse_samples_target: 16,
                diffuse_samples_availability:
                    fightbox_steam_audio::ReflectionSettingAvailability::Implemented,
                bounces: 1,
                ir_duration_s: 0.5,
                cadence_divisor: 2,
            },
            pathing: PathQualityLevel::NoValidation,
            ambisonic_order: 0,
            reverb: ReverbStrategy::ShortIrLowerOrder,
            reflection_output_gain: 1.0,
            boot_reflection_level: ReflectionQualityLevel::Reduced,
            boot_predicted_cost_ns: 0,
            boot_p99_budget_ns: 0,
            boot_cost_limit_ns: 0,
            sources: [fightbox_steam_audio::SourceQualityTelemetry::default(); MAX_ACTIVE_SOURCES],
            source_count: 1,
            memory: fightbox_steam_audio::SessionMemoryTelemetry {
                tracked_at_create_bytes: 100,
                tracked_current_bytes: 90,
                tracked_peak_bytes: 110,
                snapshot_ring_payload_bytes: 10,
                reflection_ir_payload_capacity_bytes: 20,
                audio_buffer_payload_bytes: 15,
                render_scratch_bytes: 5,
                propagation_delay_line_bytes: 30,
                retained_bake_bytes: 10,
                steam_audio_sdk_internal: MemoryTrackingStatus::Untracked,
            },
        };
        let value: serde_json::Value =
            serde_json::from_str(&telemetry_json(Some(telemetry), 8)).unwrap();

        assert_eq!(value["quality_tier"], "mobile");
        assert_eq!(value["memory"]["tracked_at_create_bytes"], 108);
        assert_eq!(value["memory"]["tracked_current_bytes"], 98);
        assert_eq!(value["memory"]["tracked_peak_bytes"], 118);
        assert_eq!(
            value["memory"]["untracked"][0]["category"],
            "steam_audio_sdk_internal"
        );
        assert_eq!(value["memory"]["untracked"][0]["status"], "untracked");
    }
}
