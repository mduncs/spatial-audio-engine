//! Audited Steam Audio 4.8.1 C ABI subset.
//!
//! Layouts and enum values in this module are transcribed from the exact
//! `v4.8.1` `core/src/core/phonon.h`. Every foreign declaration, unsafe call,
//! pointer dereference, and C callback is contained in this module.

#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use core::ffi::{c_char, c_int, c_uint, c_void};
use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

pub const IPL_STATUS_SUCCESS: c_int = 0;
pub const IPL_FALSE: c_int = 0;
pub const IPL_TRUE: c_int = 1;
pub const IPL_SCENETYPE_DEFAULT: c_int = 0;
pub const IPL_HRTFTYPE_DEFAULT: c_int = 0;
pub const IPL_HRTFNORMTYPE_NONE: c_int = 0;
pub const IPL_HRTFINTERPOLATION_BILINEAR: c_int = 1;
pub const IPL_SPEAKERLAYOUTTYPE_STEREO: c_int = 1;
pub const IPL_REFLECTIONEFFECTTYPE_CONVOLUTION: c_int = 0;
pub const IPL_REFLECTIONEFFECTTYPE_PARAMETRIC: c_int = 1;
pub const IPL_REFLECTIONEFFECTTYPE_HYBRID: c_int = 2;
pub const IPL_REFLECTIONEFFECTTYPE_TAN: c_int = 3;
pub const IPL_PROBEGENERATIONTYPE_UNIFORMFLOOR: c_int = 1;
pub const IPL_BAKEDDATAVARIATION_DYNAMIC: c_int = 3;
pub const IPL_BAKEDDATATYPE_PATHING: c_int = 1;
pub const IPL_SIMULATIONFLAGS_DIRECT: c_int = 1 << 0;
pub const IPL_SIMULATIONFLAGS_REFLECTIONS: c_int = 1 << 1;
pub const IPL_SIMULATIONFLAGS_PATHING: c_int = 1 << 2;
pub const IPL_DIRECTSIMULATIONFLAGS_DISTANCEATTENUATION: c_int = 1 << 0;
pub const IPL_DIRECTSIMULATIONFLAGS_AIRABSORPTION: c_int = 1 << 1;
pub const IPL_DIRECTSIMULATIONFLAGS_DIRECTIVITY: c_int = 1 << 2;
pub const IPL_DIRECTSIMULATIONFLAGS_OCCLUSION: c_int = 1 << 3;
pub const IPL_DIRECTSIMULATIONFLAGS_TRANSMISSION: c_int = 1 << 4;
pub const IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION: c_int = 1 << 0;
pub const IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION: c_int = 1 << 1;
pub const IPL_DIRECTEFFECTFLAGS_APPLYDIRECTIVITY: c_int = 1 << 2;
pub const IPL_DIRECTEFFECTFLAGS_APPLYOCCLUSION: c_int = 1 << 3;
pub const IPL_DIRECTEFFECTFLAGS_APPLYTRANSMISSION: c_int = 1 << 4;
pub const IPL_TRANSMISSIONTYPE_FREQDEPENDENT: c_int = 1;
pub const IPL_DISTANCEATTENUATIONTYPE_DEFAULT: c_int = 0;
pub const IPL_AIRABSORPTIONTYPE_DEFAULT: c_int = 0;
pub const IPL_DEVIATIONTYPE_DEFAULT: c_int = 0;
pub const IPL_OCCLUSIONTYPE_RAYCAST: c_int = 0;
pub const IPL_OCCLUSIONTYPE_VOLUMETRIC: c_int = 1;

const STEAM_AUDIO_PACKED_VERSION: c_uint = (4 << 16) | (8 << 8) | 1;

macro_rules! opaque_handle {
    ($opaque:ident, $alias:ident) => {
        #[repr(C)]
        pub struct $opaque {
            _private: [u8; 0],
        }
        pub type $alias = *mut $opaque;
    };
}

opaque_handle!(IPLContextOpaque, IPLContext);
opaque_handle!(IPLSerializedObjectOpaque, IPLSerializedObject);
opaque_handle!(IPLSceneOpaque, IPLScene);
opaque_handle!(IPLStaticMeshOpaque, IPLStaticMesh);
opaque_handle!(IPLProbeArrayOpaque, IPLProbeArray);
opaque_handle!(IPLProbeBatchOpaque, IPLProbeBatch);
opaque_handle!(IPLHRTFOpaque, IPLHRTF);
opaque_handle!(IPLDirectEffectOpaque, IPLDirectEffect);
opaque_handle!(IPLBinauralEffectOpaque, IPLBinauralEffect);
opaque_handle!(IPLPathEffectOpaque, IPLPathEffect);
opaque_handle!(IPLReflectionEffectOpaque, IPLReflectionEffect);
opaque_handle!(
    IPLAmbisonicsBinauralEffectOpaque,
    IPLAmbisonicsBinauralEffect
);
opaque_handle!(IPLAmbisonicsDecodeEffectOpaque, IPLAmbisonicsDecodeEffect);
opaque_handle!(IPLReflectionEffectIROpaque, IPLReflectionEffectIR);
opaque_handle!(IPLReflectionMixerOpaque, IPLReflectionMixer);
opaque_handle!(IPLSimulatorOpaque, IPLSimulator);
opaque_handle!(IPLSourceOpaque, IPLSource);

pub type IPLEmbreeDevice = *mut c_void;
pub type IPLRadeonRaysDevice = *mut c_void;
pub type IPLOpenCLDevice = *mut c_void;
pub type IPLTrueAudioNextDevice = *mut c_void;

pub type IPLLogFunction = Option<unsafe extern "system" fn(c_int, *const c_char)>;
pub type IPLAllocateFunction = Option<unsafe extern "system" fn(usize, usize) -> *mut c_void>;
pub type IPLFreeFunction = Option<unsafe extern "system" fn(*mut c_void)>;

#[repr(C)]
pub struct IPLContextSettings {
    pub version: c_uint,
    pub logCallback: IPLLogFunction,
    pub allocateCallback: IPLAllocateFunction,
    pub freeCallback: IPLFreeFunction,
    pub simdLevel: c_int,
    pub flags: c_int,
}

impl IPLContextSettings {
    pub const fn pinned_defaults() -> Self {
        Self {
            version: STEAM_AUDIO_PACKED_VERSION,
            logCallback: None,
            allocateCallback: None,
            freeCallback: None,
            // IPL_SIMDLEVEL_SSE2, also the ABI value for NEON.
            simdLevel: 0,
            flags: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLVector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLMatrix4x4 {
    pub elements: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLSphere {
    pub center: IPLVector3,
    pub radius: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLCoordinateSpace3 {
    pub right: IPLVector3,
    pub up: IPLVector3,
    pub ahead: IPLVector3,
    pub origin: IPLVector3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLSerializedObjectSettings {
    pub data: *mut u8,
    pub size: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLTriangle {
    pub indices: [c_int; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLMaterial {
    pub absorption: [f32; 3],
    pub scattering: f32,
    pub transmission: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLRay {
    pub origin: IPLVector3,
    pub direction: IPLVector3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLHit {
    pub distance: f32,
    pub triangleIndex: c_int,
    pub objectIndex: c_int,
    pub materialIndex: c_int,
    pub normal: IPLVector3,
    pub material: *mut IPLMaterial,
}

pub type IPLClosestHitCallback =
    Option<unsafe extern "system" fn(*const IPLRay, f32, f32, *mut IPLHit, *mut c_void)>;
pub type IPLAnyHitCallback =
    Option<unsafe extern "system" fn(*const IPLRay, f32, f32, *mut u8, *mut c_void)>;
pub type IPLBatchedClosestHitCallback = Option<
    unsafe extern "system" fn(
        c_int,
        *const IPLRay,
        *const f32,
        *const f32,
        *mut IPLHit,
        *mut c_void,
    ),
>;
pub type IPLBatchedAnyHitCallback = Option<
    unsafe extern "system" fn(c_int, *const IPLRay, *const f32, *const f32, *mut u8, *mut c_void),
>;

#[repr(C)]
pub struct IPLSceneSettings {
    pub type_: c_int,
    pub closestHitCallback: IPLClosestHitCallback,
    pub anyHitCallback: IPLAnyHitCallback,
    pub batchedClosestHitCallback: IPLBatchedClosestHitCallback,
    pub batchedAnyHitCallback: IPLBatchedAnyHitCallback,
    pub userData: *mut c_void,
    pub embreeDevice: IPLEmbreeDevice,
    pub radeonRaysDevice: IPLRadeonRaysDevice,
}

#[repr(C)]
pub struct IPLStaticMeshSettings {
    pub numVertices: c_int,
    pub numTriangles: c_int,
    pub numMaterials: c_int,
    pub vertices: *mut IPLVector3,
    pub triangles: *mut IPLTriangle,
    pub materialIndices: *mut c_int,
    pub materials: *mut IPLMaterial,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLSpeakerLayout {
    pub type_: c_int,
    pub numSpeakers: c_int,
    pub speakers: *mut IPLVector3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLAudioSettings {
    pub samplingRate: c_int,
    pub frameSize: c_int,
}

#[repr(C)]
#[derive(Debug)]
pub struct IPLAudioBuffer {
    pub numChannels: c_int,
    pub numSamples: c_int,
    pub data: *mut *mut f32,
}

#[repr(C)]
pub struct IPLHRTFSettings {
    pub type_: c_int,
    pub sofaFileName: *const c_char,
    pub sofaData: *const u8,
    pub sofaDataSize: c_int,
    pub volume: f32,
    pub normType: c_int,
}

#[repr(C)]
pub struct IPLBinauralEffectSettings {
    pub hrtf: IPLHRTF,
}

#[repr(C)]
pub struct IPLBinauralEffectParams {
    pub direction: IPLVector3,
    pub interpolation: c_int,
    pub spatialBlend: f32,
    pub hrtf: IPLHRTF,
    pub peakDelays: *mut f32,
}

#[repr(C)]
pub struct IPLAmbisonicsBinauralEffectSettings {
    pub hrtf: IPLHRTF,
    pub maxOrder: c_int,
}

#[repr(C)]
pub struct IPLAmbisonicsBinauralEffectParams {
    pub hrtf: IPLHRTF,
    pub order: c_int,
}

#[repr(C)]
pub struct IPLAmbisonicsDecodeEffectSettings {
    pub speakerLayout: IPLSpeakerLayout,
    pub hrtf: IPLHRTF,
    pub maxOrder: c_int,
}

#[repr(C)]
pub struct IPLAmbisonicsDecodeEffectParams {
    pub order: c_int,
    pub hrtf: IPLHRTF,
    pub orientation: IPLCoordinateSpace3,
    pub binaural: c_int,
}

#[repr(C)]
pub struct IPLDirectEffectSettings {
    pub numChannels: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLDirectEffectParams {
    pub flags: c_int,
    pub transmissionType: c_int,
    pub distanceAttenuation: f32,
    pub airAbsorption: [f32; 3],
    pub directivity: f32,
    pub occlusion: f32,
    pub transmission: [f32; 3],
}

#[repr(C)]
pub struct IPLReflectionEffectSettings {
    pub type_: c_int,
    pub irSize: c_int,
    pub numChannels: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLReflectionEffectParams {
    pub type_: c_int,
    pub ir: IPLReflectionEffectIR,
    pub reverbTimes: [f32; 3],
    pub eq: [f32; 3],
    pub delay: c_int,
    pub numChannels: c_int,
    pub irSize: c_int,
    pub tanDevice: IPLTrueAudioNextDevice,
    pub tanSlot: c_int,
}

#[repr(C)]
pub struct IPLPathEffectSettings {
    pub maxOrder: c_int,
    pub spatialize: c_int,
    pub speakerLayout: IPLSpeakerLayout,
    pub hrtf: IPLHRTF,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLPathEffectParams {
    pub eqCoeffs: [f32; 3],
    pub shCoeffs: *mut f32,
    pub order: c_int,
    pub binaural: c_int,
    pub hrtf: IPLHRTF,
    pub listener: IPLCoordinateSpace3,
    pub normalizeEQ: c_int,
}

#[repr(C)]
pub struct IPLProbeGenerationParams {
    pub type_: c_int,
    pub spacing: f32,
    pub height: f32,
    pub transform: IPLMatrix4x4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IPLBakedDataIdentifier {
    pub type_: c_int,
    pub variation: c_int,
    pub endpointInfluence: IPLSphere,
}

#[repr(C)]
pub struct IPLPathBakeParams {
    pub scene: IPLScene,
    pub probeBatch: IPLProbeBatch,
    pub identifier: IPLBakedDataIdentifier,
    pub numSamples: c_int,
    pub radius: f32,
    pub threshold: f32,
    pub visRange: f32,
    pub pathRange: f32,
    pub numThreads: c_int,
}

pub type IPLDistanceAttenuationCallback =
    Option<unsafe extern "system" fn(f32, *mut c_void) -> f32>;
pub type IPLAirAbsorptionCallback =
    Option<unsafe extern "system" fn(f32, c_int, *mut c_void) -> f32>;
pub type IPLDirectivityCallback = Option<unsafe extern "system" fn(IPLVector3, *mut c_void) -> f32>;
pub type IPLDeviationCallback = Option<unsafe extern "system" fn(f32, c_int, *mut c_void) -> f32>;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLDistanceAttenuationModel {
    pub type_: c_int,
    pub minDistance: f32,
    pub callback: IPLDistanceAttenuationCallback,
    pub userData: *mut c_void,
    pub dirty: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLAirAbsorptionModel {
    pub type_: c_int,
    pub coefficients: [f32; 3],
    pub callback: IPLAirAbsorptionCallback,
    pub userData: *mut c_void,
    pub dirty: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLDirectivity {
    pub dipoleWeight: f32,
    pub dipolePower: f32,
    pub callback: IPLDirectivityCallback,
    pub userData: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IPLDeviationModel {
    pub type_: c_int,
    pub callback: IPLDeviationCallback,
    pub userData: *mut c_void,
}

#[repr(C)]
pub struct IPLSimulationSettings {
    pub flags: c_int,
    pub sceneType: c_int,
    pub reflectionType: c_int,
    pub maxNumOcclusionSamples: c_int,
    pub maxNumRays: c_int,
    pub numDiffuseSamples: c_int,
    pub maxDuration: f32,
    pub maxOrder: c_int,
    pub maxNumSources: c_int,
    pub numThreads: c_int,
    pub rayBatchSize: c_int,
    pub numVisSamples: c_int,
    pub samplingRate: c_int,
    pub frameSize: c_int,
    pub openCLDevice: IPLOpenCLDevice,
    pub radeonRaysDevice: IPLRadeonRaysDevice,
    pub tanDevice: IPLTrueAudioNextDevice,
}

#[repr(C)]
pub struct IPLSourceSettings {
    pub flags: c_int,
}

#[repr(C)]
pub struct IPLSimulationInputs {
    pub flags: c_int,
    pub directFlags: c_int,
    pub source: IPLCoordinateSpace3,
    pub distanceAttenuationModel: IPLDistanceAttenuationModel,
    pub airAbsorptionModel: IPLAirAbsorptionModel,
    pub directivity: IPLDirectivity,
    pub occlusionType: c_int,
    pub occlusionRadius: f32,
    pub numOcclusionSamples: c_int,
    pub reverbScale: [f32; 3],
    pub hybridReverbTransitionTime: f32,
    pub hybridReverbOverlapPercent: f32,
    pub baked: c_int,
    pub bakedDataIdentifier: IPLBakedDataIdentifier,
    pub pathingProbes: IPLProbeBatch,
    pub visRadius: f32,
    pub visThreshold: f32,
    pub visRange: f32,
    pub pathingOrder: c_int,
    pub enableValidation: c_int,
    pub findAlternatePaths: c_int,
    pub numTransmissionRays: c_int,
    pub deviationModel: *mut IPLDeviationModel,
}

pub type IPLPathingVisualizationCallback =
    Option<unsafe extern "system" fn(IPLVector3, IPLVector3, c_int, *mut c_void)>;

#[derive(Clone, Copy, Debug)]
pub struct PathValidationSegment {
    pub from: IPLVector3,
    pub to: IPLVector3,
    pub occluded: bool,
}

#[derive(Debug, Default)]
pub struct PathValidationTrace {
    segments: Mutex<Vec<PathValidationSegment>>,
}

impl PathValidationTrace {
    pub fn into_segments(self) -> Vec<PathValidationSegment> {
        match self.segments.into_inner() {
            Ok(segments) => segments,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

unsafe extern "system" fn record_path_validation_segment(
    from: IPLVector3,
    to: IPLVector3,
    occluded: c_int,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `path_validation_trace_callback` is installed only while a live
    // `PathValidationTrace` is passed to the blocking `iplSimulatorRunPathing`
    // call. The callback is cleared before that local is dropped.
    let trace = unsafe { &*user_data.cast::<PathValidationTrace>() };
    if let Ok(mut segments) = trace.segments.lock() {
        segments.push(PathValidationSegment {
            from,
            to,
            occluded: occluded != IPL_FALSE,
        });
    }
}

pub fn path_validation_trace_callback() -> IPLPathingVisualizationCallback {
    Some(record_path_validation_segment)
}

pub fn path_validation_trace_user_data(trace: &mut PathValidationTrace) -> *mut c_void {
    core::ptr::from_mut(trace).cast()
}

#[repr(C)]
pub struct IPLSimulationSharedInputs {
    pub listener: IPLCoordinateSpace3,
    pub numRays: c_int,
    pub numBounces: c_int,
    pub duration: f32,
    pub order: c_int,
    pub irradianceMinDistance: f32,
    pub pathingVisCallback: IPLPathingVisualizationCallback,
    pub pathingUserData: *mut c_void,
}

#[repr(C)]
pub struct IPLSimulationOutputs {
    pub direct: IPLDirectEffectParams,
    pub reflections: IPLReflectionEffectParams,
    pub pathing: IPLPathEffectParams,
}

impl IPLSimulationOutputs {
    pub fn zeroed() -> Self {
        Self {
            direct: IPLDirectEffectParams::default(),
            reflections: IPLReflectionEffectParams {
                type_: 0,
                ir: core::ptr::null_mut(),
                reverbTimes: [0.0; 3],
                eq: [0.0; 3],
                delay: 0,
                numChannels: 0,
                irSize: 0,
                tanDevice: core::ptr::null_mut(),
                tanSlot: 0,
            },
            pathing: IPLPathEffectParams {
                eqCoeffs: [0.0; 3],
                shCoeffs: core::ptr::null_mut(),
                order: 0,
                binaural: 0,
                hrtf: core::ptr::null_mut(),
                listener: IPLCoordinateSpace3::default(),
                normalizeEQ: 0,
            },
        }
    }
}

/// Summary of progress callbacks observed during one blocking bake.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BakeProgress {
    pub callback_count: u32,
    pub final_fraction: f32,
}

struct BakeProgressState {
    callback_count: AtomicU32,
    final_fraction_bits: AtomicU32,
}

unsafe extern "system" fn record_bake_progress(progress: f32, user_data: *mut c_void) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `path_baker_bake` passes a pointer to a live `BakeProgressState`
    // and the blocking SDK call has not returned while this callback runs.
    let state = unsafe { &*(user_data.cast::<BakeProgressState>()) };
    state.callback_count.fetch_add(1, Ordering::Relaxed);
    state
        .final_fraction_bits
        .store(progress.to_bits(), Ordering::Relaxed);
}

#[link(name = "phonon")]
unsafe extern "system" {
    #[link_name = "iplContextCreate"]
    fn raw_context_create(settings: *mut IPLContextSettings, context: *mut IPLContext) -> c_int;
    #[link_name = "iplContextRelease"]
    fn raw_context_release(context: *mut IPLContext);

    #[link_name = "iplSerializedObjectCreate"]
    fn raw_serialized_object_create(
        context: IPLContext,
        settings: *mut IPLSerializedObjectSettings,
        serialized_object: *mut IPLSerializedObject,
    ) -> c_int;
    #[link_name = "iplSerializedObjectRelease"]
    fn raw_serialized_object_release(serialized_object: *mut IPLSerializedObject);
    #[link_name = "iplSerializedObjectGetSize"]
    fn raw_serialized_object_get_size(serialized_object: IPLSerializedObject) -> usize;
    #[link_name = "iplSerializedObjectGetData"]
    fn raw_serialized_object_get_data(serialized_object: IPLSerializedObject) -> *mut u8;

    #[link_name = "iplSceneCreate"]
    fn raw_scene_create(
        context: IPLContext,
        settings: *mut IPLSceneSettings,
        scene: *mut IPLScene,
    ) -> c_int;
    #[link_name = "iplSceneRelease"]
    fn raw_scene_release(scene: *mut IPLScene);
    #[link_name = "iplSceneCommit"]
    fn raw_scene_commit(scene: IPLScene);
    #[link_name = "iplStaticMeshCreate"]
    fn raw_static_mesh_create(
        scene: IPLScene,
        settings: *mut IPLStaticMeshSettings,
        static_mesh: *mut IPLStaticMesh,
    ) -> c_int;
    #[link_name = "iplStaticMeshRelease"]
    fn raw_static_mesh_release(static_mesh: *mut IPLStaticMesh);
    #[link_name = "iplStaticMeshAdd"]
    fn raw_static_mesh_add(static_mesh: IPLStaticMesh, scene: IPLScene);
    #[link_name = "iplStaticMeshRemove"]
    fn raw_static_mesh_remove(static_mesh: IPLStaticMesh, scene: IPLScene);

    #[link_name = "iplAudioBufferAllocate"]
    fn raw_audio_buffer_allocate(
        context: IPLContext,
        num_channels: c_int,
        num_samples: c_int,
        audio_buffer: *mut IPLAudioBuffer,
    ) -> c_int;
    #[link_name = "iplAudioBufferFree"]
    fn raw_audio_buffer_free(context: IPLContext, audio_buffer: *mut IPLAudioBuffer);
    #[link_name = "iplAudioBufferInterleave"]
    fn raw_audio_buffer_interleave(context: IPLContext, src: *mut IPLAudioBuffer, dst: *mut f32);
    #[link_name = "iplAudioBufferDeinterleave"]
    fn raw_audio_buffer_deinterleave(context: IPLContext, src: *mut f32, dst: *mut IPLAudioBuffer);

    #[link_name = "iplHRTFCreate"]
    fn raw_hrtf_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        hrtf_settings: *mut IPLHRTFSettings,
        hrtf: *mut IPLHRTF,
    ) -> c_int;
    #[link_name = "iplHRTFRelease"]
    fn raw_hrtf_release(hrtf: *mut IPLHRTF);

    #[link_name = "iplDirectEffectCreate"]
    fn raw_direct_effect_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        effect_settings: *mut IPLDirectEffectSettings,
        effect: *mut IPLDirectEffect,
    ) -> c_int;
    #[link_name = "iplDirectEffectRelease"]
    fn raw_direct_effect_release(effect: *mut IPLDirectEffect);
    #[link_name = "iplDirectEffectApply"]
    fn raw_direct_effect_apply(
        effect: IPLDirectEffect,
        params: *mut IPLDirectEffectParams,
        input: *mut IPLAudioBuffer,
        output: *mut IPLAudioBuffer,
    ) -> c_int;

    #[link_name = "iplBinauralEffectCreate"]
    fn raw_binaural_effect_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        effect_settings: *mut IPLBinauralEffectSettings,
        effect: *mut IPLBinauralEffect,
    ) -> c_int;
    #[link_name = "iplBinauralEffectRelease"]
    fn raw_binaural_effect_release(effect: *mut IPLBinauralEffect);
    #[link_name = "iplBinauralEffectApply"]
    fn raw_binaural_effect_apply(
        effect: IPLBinauralEffect,
        params: *mut IPLBinauralEffectParams,
        input: *mut IPLAudioBuffer,
        output: *mut IPLAudioBuffer,
    ) -> c_int;

    #[link_name = "iplPathEffectCreate"]
    fn raw_path_effect_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        effect_settings: *mut IPLPathEffectSettings,
        effect: *mut IPLPathEffect,
    ) -> c_int;
    #[link_name = "iplPathEffectRelease"]
    fn raw_path_effect_release(effect: *mut IPLPathEffect);
    #[link_name = "iplPathEffectApply"]
    fn raw_path_effect_apply(
        effect: IPLPathEffect,
        params: *mut IPLPathEffectParams,
        input: *mut IPLAudioBuffer,
        output: *mut IPLAudioBuffer,
    ) -> c_int;

    #[link_name = "iplReflectionEffectCreate"]
    fn raw_reflection_effect_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        effect_settings: *mut IPLReflectionEffectSettings,
        effect: *mut IPLReflectionEffect,
    ) -> c_int;
    #[link_name = "iplReflectionEffectRelease"]
    fn raw_reflection_effect_release(effect: *mut IPLReflectionEffect);
    #[link_name = "iplReflectionEffectApply"]
    fn raw_reflection_effect_apply(
        effect: IPLReflectionEffect,
        params: *mut IPLReflectionEffectParams,
        input: *mut IPLAudioBuffer,
        output: *mut IPLAudioBuffer,
        mixer: IPLReflectionMixer,
    ) -> c_int;
    #[link_name = "iplReflectionMixerCreate"]
    fn raw_reflection_mixer_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        effect_settings: *mut IPLReflectionEffectSettings,
        mixer: *mut IPLReflectionMixer,
    ) -> c_int;
    #[link_name = "iplReflectionMixerRelease"]
    fn raw_reflection_mixer_release(mixer: *mut IPLReflectionMixer);
    #[link_name = "iplReflectionMixerApply"]
    fn raw_reflection_mixer_apply(
        mixer: IPLReflectionMixer,
        params: *mut IPLReflectionEffectParams,
        output: *mut IPLAudioBuffer,
    ) -> c_int;

    #[link_name = "iplAmbisonicsBinauralEffectCreate"]
    fn raw_ambisonics_binaural_effect_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        effect_settings: *mut IPLAmbisonicsBinauralEffectSettings,
        effect: *mut IPLAmbisonicsBinauralEffect,
    ) -> c_int;
    #[link_name = "iplAmbisonicsBinauralEffectRelease"]
    fn raw_ambisonics_binaural_effect_release(effect: *mut IPLAmbisonicsBinauralEffect);
    #[link_name = "iplAmbisonicsBinauralEffectApply"]
    fn raw_ambisonics_binaural_effect_apply(
        effect: IPLAmbisonicsBinauralEffect,
        params: *mut IPLAmbisonicsBinauralEffectParams,
        input: *mut IPLAudioBuffer,
        output: *mut IPLAudioBuffer,
    ) -> c_int;
    #[link_name = "iplAmbisonicsDecodeEffectCreate"]
    fn raw_ambisonics_decode_effect_create(
        context: IPLContext,
        audio_settings: *mut IPLAudioSettings,
        effect_settings: *mut IPLAmbisonicsDecodeEffectSettings,
        effect: *mut IPLAmbisonicsDecodeEffect,
    ) -> c_int;
    #[link_name = "iplAmbisonicsDecodeEffectRelease"]
    fn raw_ambisonics_decode_effect_release(effect: *mut IPLAmbisonicsDecodeEffect);
    #[link_name = "iplAmbisonicsDecodeEffectApply"]
    fn raw_ambisonics_decode_effect_apply(
        effect: IPLAmbisonicsDecodeEffect,
        params: *mut IPLAmbisonicsDecodeEffectParams,
        input: *mut IPLAudioBuffer,
        output: *mut IPLAudioBuffer,
    ) -> c_int;

    #[link_name = "iplProbeArrayCreate"]
    fn raw_probe_array_create(context: IPLContext, probe_array: *mut IPLProbeArray) -> c_int;
    #[link_name = "iplProbeArrayRelease"]
    fn raw_probe_array_release(probe_array: *mut IPLProbeArray);
    #[link_name = "iplProbeArrayGenerateProbes"]
    fn raw_probe_array_generate_probes(
        probe_array: IPLProbeArray,
        scene: IPLScene,
        params: *mut IPLProbeGenerationParams,
    );
    #[link_name = "iplProbeArrayGetNumProbes"]
    fn raw_probe_array_get_num_probes(probe_array: IPLProbeArray) -> c_int;

    #[link_name = "iplProbeBatchCreate"]
    fn raw_probe_batch_create(context: IPLContext, probe_batch: *mut IPLProbeBatch) -> c_int;
    #[link_name = "iplProbeBatchLoad"]
    fn raw_probe_batch_load(
        context: IPLContext,
        serialized_object: IPLSerializedObject,
        probe_batch: *mut IPLProbeBatch,
    ) -> c_int;
    #[link_name = "iplProbeBatchSave"]
    fn raw_probe_batch_save(probe_batch: IPLProbeBatch, serialized_object: IPLSerializedObject);
    #[link_name = "iplProbeBatchRelease"]
    fn raw_probe_batch_release(probe_batch: *mut IPLProbeBatch);
    #[link_name = "iplProbeBatchGetNumProbes"]
    fn raw_probe_batch_get_num_probes(probe_batch: IPLProbeBatch) -> c_int;
    #[link_name = "iplProbeBatchAddProbe"]
    fn raw_probe_batch_add_probe(probe_batch: IPLProbeBatch, probe: IPLSphere);
    #[link_name = "iplProbeBatchAddProbeArray"]
    fn raw_probe_batch_add_probe_array(probe_batch: IPLProbeBatch, probe_array: IPLProbeArray);
    #[link_name = "iplProbeBatchCommit"]
    fn raw_probe_batch_commit(probe_batch: IPLProbeBatch);
    #[link_name = "iplProbeBatchGetDataSize"]
    fn raw_probe_batch_get_data_size(
        probe_batch: IPLProbeBatch,
        identifier: *mut IPLBakedDataIdentifier,
    ) -> usize;

    #[link_name = "iplPathBakerBake"]
    fn raw_path_baker_bake(
        context: IPLContext,
        params: *mut IPLPathBakeParams,
        progress_callback: Option<unsafe extern "system" fn(f32, *mut c_void)>,
        user_data: *mut c_void,
    );

    #[link_name = "iplSimulatorCreate"]
    fn raw_simulator_create(
        context: IPLContext,
        settings: *mut IPLSimulationSettings,
        simulator: *mut IPLSimulator,
    ) -> c_int;
    #[link_name = "iplSimulatorRelease"]
    fn raw_simulator_release(simulator: *mut IPLSimulator);
    #[link_name = "iplSimulatorSetScene"]
    fn raw_simulator_set_scene(simulator: IPLSimulator, scene: IPLScene);
    #[link_name = "iplSimulatorAddProbeBatch"]
    fn raw_simulator_add_probe_batch(simulator: IPLSimulator, probe_batch: IPLProbeBatch);
    #[link_name = "iplSimulatorRemoveProbeBatch"]
    fn raw_simulator_remove_probe_batch(simulator: IPLSimulator, probe_batch: IPLProbeBatch);
    #[link_name = "iplSimulatorSetSharedInputs"]
    fn raw_simulator_set_shared_inputs(
        simulator: IPLSimulator,
        flags: c_int,
        shared_inputs: *mut IPLSimulationSharedInputs,
    );
    #[link_name = "iplSimulatorCommit"]
    fn raw_simulator_commit(simulator: IPLSimulator);
    #[link_name = "iplSimulatorRunDirect"]
    fn raw_simulator_run_direct(simulator: IPLSimulator);
    #[link_name = "iplSimulatorRunReflections"]
    fn raw_simulator_run_reflections(simulator: IPLSimulator);
    #[link_name = "iplSimulatorRunPathing"]
    fn raw_simulator_run_pathing(simulator: IPLSimulator);

    #[link_name = "iplSourceCreate"]
    fn raw_source_create(
        simulator: IPLSimulator,
        settings: *mut IPLSourceSettings,
        source: *mut IPLSource,
    ) -> c_int;
    #[link_name = "iplSourceRelease"]
    fn raw_source_release(source: *mut IPLSource);
    #[link_name = "iplSourceAdd"]
    fn raw_source_add(source: IPLSource, simulator: IPLSimulator);
    #[link_name = "iplSourceRemove"]
    fn raw_source_remove(source: IPLSource, simulator: IPLSimulator);
    #[link_name = "iplSourceSetInputs"]
    fn raw_source_set_inputs(source: IPLSource, flags: c_int, inputs: *mut IPLSimulationInputs);
    #[link_name = "iplSourceGetOutputs"]
    fn raw_source_get_outputs(source: IPLSource, flags: c_int, outputs: *mut IPLSimulationOutputs);

    #[link_name = "iplDistanceAttenuationCalculate"]
    fn raw_distance_attenuation_calculate(
        context: IPLContext,
        source: IPLVector3,
        listener: IPLVector3,
        model: *mut IPLDistanceAttenuationModel,
    ) -> f32;
    #[link_name = "iplAirAbsorptionCalculate"]
    fn raw_air_absorption_calculate(
        context: IPLContext,
        source: IPLVector3,
        listener: IPLVector3,
        model: *mut IPLAirAbsorptionModel,
        air_absorption: *mut f32,
    );
}

pub fn context_create(settings: &mut IPLContextSettings, context: &mut IPLContext) -> c_int {
    // SAFETY: pointers refer to live, correctly laid out Rust values for the duration of the call.
    unsafe { raw_context_create(settings, context) }
}

pub fn context_release(context: &mut IPLContext) {
    // SAFETY: the RAII wrapper calls this once for its live handle.
    unsafe { raw_context_release(context) }
}

pub fn serialized_object_create(
    context: IPLContext,
    settings: &mut IPLSerializedObjectSettings,
    serialized_object: &mut IPLSerializedObject,
) -> c_int {
    unsafe { raw_serialized_object_create(context, settings, serialized_object) }
}

pub fn serialized_object_release(serialized_object: &mut IPLSerializedObject) {
    unsafe { raw_serialized_object_release(serialized_object) }
}

pub fn serialized_object_copy_bytes(serialized_object: IPLSerializedObject) -> Vec<u8> {
    let size = unsafe { raw_serialized_object_get_size(serialized_object) };
    let data = unsafe { raw_serialized_object_get_data(serialized_object) };
    if size == 0 || data.is_null() {
        return Vec::new();
    }
    // SAFETY: Steam Audio promises `size` readable bytes until this serialized object is released.
    unsafe { core::slice::from_raw_parts(data.cast_const(), size) }.to_vec()
}

pub fn scene_create(
    context: IPLContext,
    settings: &mut IPLSceneSettings,
    scene: &mut IPLScene,
) -> c_int {
    unsafe { raw_scene_create(context, settings, scene) }
}

pub fn scene_release(scene: &mut IPLScene) {
    unsafe { raw_scene_release(scene) }
}

pub fn scene_commit(scene: IPLScene) {
    unsafe { raw_scene_commit(scene) }
}

pub fn static_mesh_create(
    scene: IPLScene,
    settings: &mut IPLStaticMeshSettings,
    static_mesh: &mut IPLStaticMesh,
) -> c_int {
    unsafe { raw_static_mesh_create(scene, settings, static_mesh) }
}

pub fn static_mesh_release(static_mesh: &mut IPLStaticMesh) {
    unsafe { raw_static_mesh_release(static_mesh) }
}

pub fn static_mesh_add(static_mesh: IPLStaticMesh, scene: IPLScene) {
    unsafe { raw_static_mesh_add(static_mesh, scene) }
}

pub fn static_mesh_remove(static_mesh: IPLStaticMesh, scene: IPLScene) {
    unsafe { raw_static_mesh_remove(static_mesh, scene) }
}

pub fn audio_buffer_allocate(
    context: IPLContext,
    num_channels: c_int,
    num_samples: c_int,
    audio_buffer: &mut IPLAudioBuffer,
) -> c_int {
    unsafe { raw_audio_buffer_allocate(context, num_channels, num_samples, audio_buffer) }
}

pub fn audio_buffer_free(context: IPLContext, audio_buffer: &mut IPLAudioBuffer) {
    unsafe { raw_audio_buffer_free(context, audio_buffer) }
}

pub fn audio_buffer_interleave(context: IPLContext, src: &mut IPLAudioBuffer, dst: &mut [f32]) {
    debug_assert_eq!(
        dst.len(),
        src.numChannels as usize * src.numSamples as usize
    );
    unsafe { raw_audio_buffer_interleave(context, src, dst.as_mut_ptr()) }
}

pub fn audio_buffer_deinterleave(context: IPLContext, src: &mut [f32], dst: &mut IPLAudioBuffer) {
    debug_assert_eq!(
        src.len(),
        dst.numChannels as usize * dst.numSamples as usize
    );
    unsafe { raw_audio_buffer_deinterleave(context, src.as_mut_ptr(), dst) }
}

pub fn hrtf_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    hrtf_settings: &mut IPLHRTFSettings,
    hrtf: &mut IPLHRTF,
) -> c_int {
    unsafe { raw_hrtf_create(context, audio_settings, hrtf_settings, hrtf) }
}

pub fn hrtf_release(hrtf: &mut IPLHRTF) {
    unsafe { raw_hrtf_release(hrtf) }
}

pub fn direct_effect_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    effect_settings: &mut IPLDirectEffectSettings,
    effect: &mut IPLDirectEffect,
) -> c_int {
    unsafe { raw_direct_effect_create(context, audio_settings, effect_settings, effect) }
}

pub fn direct_effect_release(effect: &mut IPLDirectEffect) {
    unsafe { raw_direct_effect_release(effect) }
}

pub fn direct_effect_apply(
    effect: IPLDirectEffect,
    params: &mut IPLDirectEffectParams,
    input: &mut IPLAudioBuffer,
    output: &mut IPLAudioBuffer,
) {
    let _ = unsafe { raw_direct_effect_apply(effect, params, input, output) };
}

pub fn binaural_effect_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    effect_settings: &mut IPLBinauralEffectSettings,
    effect: &mut IPLBinauralEffect,
) -> c_int {
    unsafe { raw_binaural_effect_create(context, audio_settings, effect_settings, effect) }
}

pub fn binaural_effect_release(effect: &mut IPLBinauralEffect) {
    unsafe { raw_binaural_effect_release(effect) }
}

pub fn binaural_effect_apply(
    effect: IPLBinauralEffect,
    params: &mut IPLBinauralEffectParams,
    input: &mut IPLAudioBuffer,
    output: &mut IPLAudioBuffer,
) {
    let _ = unsafe { raw_binaural_effect_apply(effect, params, input, output) };
}

pub fn path_effect_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    effect_settings: &mut IPLPathEffectSettings,
    effect: &mut IPLPathEffect,
) -> c_int {
    unsafe { raw_path_effect_create(context, audio_settings, effect_settings, effect) }
}

pub fn path_effect_release(effect: &mut IPLPathEffect) {
    unsafe { raw_path_effect_release(effect) }
}

pub fn path_effect_apply(
    effect: IPLPathEffect,
    params: &mut IPLPathEffectParams,
    input: &mut IPLAudioBuffer,
    output: &mut IPLAudioBuffer,
) {
    let _ = unsafe { raw_path_effect_apply(effect, params, input, output) };
}

pub fn reflection_effect_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    effect_settings: &mut IPLReflectionEffectSettings,
    effect: &mut IPLReflectionEffect,
) -> c_int {
    unsafe { raw_reflection_effect_create(context, audio_settings, effect_settings, effect) }
}

pub fn reflection_effect_release(effect: &mut IPLReflectionEffect) {
    unsafe { raw_reflection_effect_release(effect) }
}

pub fn reflection_effect_apply(
    effect: IPLReflectionEffect,
    params: &mut IPLReflectionEffectParams,
    input: &mut IPLAudioBuffer,
    output: &mut IPLAudioBuffer,
) {
    let _ = unsafe {
        raw_reflection_effect_apply(effect, params, input, output, core::ptr::null_mut())
    };
}

pub fn reflection_effect_apply_to_mixer(
    effect: IPLReflectionEffect,
    params: &mut IPLReflectionEffectParams,
    input: &mut IPLAudioBuffer,
    scratch_output: &mut IPLAudioBuffer,
    mixer: IPLReflectionMixer,
) {
    let _ = unsafe { raw_reflection_effect_apply(effect, params, input, scratch_output, mixer) };
}

pub fn reflection_mixer_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    effect_settings: &mut IPLReflectionEffectSettings,
    mixer: &mut IPLReflectionMixer,
) -> c_int {
    unsafe { raw_reflection_mixer_create(context, audio_settings, effect_settings, mixer) }
}

pub fn reflection_mixer_release(mixer: &mut IPLReflectionMixer) {
    unsafe { raw_reflection_mixer_release(mixer) }
}

pub fn reflection_mixer_apply(
    mixer: IPLReflectionMixer,
    params: &mut IPLReflectionEffectParams,
    output: &mut IPLAudioBuffer,
) {
    let _ = unsafe { raw_reflection_mixer_apply(mixer, params, output) };
}

pub fn ambisonics_binaural_effect_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    effect_settings: &mut IPLAmbisonicsBinauralEffectSettings,
    effect: &mut IPLAmbisonicsBinauralEffect,
) -> c_int {
    unsafe {
        raw_ambisonics_binaural_effect_create(context, audio_settings, effect_settings, effect)
    }
}

pub fn ambisonics_binaural_effect_release(effect: &mut IPLAmbisonicsBinauralEffect) {
    unsafe { raw_ambisonics_binaural_effect_release(effect) }
}

pub fn ambisonics_binaural_effect_apply(
    effect: IPLAmbisonicsBinauralEffect,
    params: &mut IPLAmbisonicsBinauralEffectParams,
    input: &mut IPLAudioBuffer,
    output: &mut IPLAudioBuffer,
) {
    let _ = unsafe { raw_ambisonics_binaural_effect_apply(effect, params, input, output) };
}

pub fn ambisonics_decode_effect_create(
    context: IPLContext,
    audio_settings: &mut IPLAudioSettings,
    effect_settings: &mut IPLAmbisonicsDecodeEffectSettings,
    effect: &mut IPLAmbisonicsDecodeEffect,
) -> c_int {
    unsafe { raw_ambisonics_decode_effect_create(context, audio_settings, effect_settings, effect) }
}

pub fn ambisonics_decode_effect_release(effect: &mut IPLAmbisonicsDecodeEffect) {
    unsafe { raw_ambisonics_decode_effect_release(effect) }
}

pub fn ambisonics_decode_effect_apply(
    effect: IPLAmbisonicsDecodeEffect,
    params: &mut IPLAmbisonicsDecodeEffectParams,
    input: &mut IPLAudioBuffer,
    output: &mut IPLAudioBuffer,
) {
    let _ = unsafe { raw_ambisonics_decode_effect_apply(effect, params, input, output) };
}

pub fn probe_array_create(context: IPLContext, probe_array: &mut IPLProbeArray) -> c_int {
    unsafe { raw_probe_array_create(context, probe_array) }
}

pub fn probe_array_release(probe_array: &mut IPLProbeArray) {
    unsafe { raw_probe_array_release(probe_array) }
}

pub fn probe_array_generate_probes(
    probe_array: IPLProbeArray,
    scene: IPLScene,
    params: &mut IPLProbeGenerationParams,
) {
    unsafe { raw_probe_array_generate_probes(probe_array, scene, params) }
}

pub fn probe_array_get_num_probes(probe_array: IPLProbeArray) -> c_int {
    unsafe { raw_probe_array_get_num_probes(probe_array) }
}

pub fn probe_batch_create(context: IPLContext, probe_batch: &mut IPLProbeBatch) -> c_int {
    unsafe { raw_probe_batch_create(context, probe_batch) }
}

pub fn probe_batch_load(
    context: IPLContext,
    serialized_object: IPLSerializedObject,
    probe_batch: &mut IPLProbeBatch,
) -> c_int {
    unsafe { raw_probe_batch_load(context, serialized_object, probe_batch) }
}

pub fn probe_batch_save(probe_batch: IPLProbeBatch, serialized_object: IPLSerializedObject) {
    unsafe { raw_probe_batch_save(probe_batch, serialized_object) }
}

pub fn probe_batch_release(probe_batch: &mut IPLProbeBatch) {
    unsafe { raw_probe_batch_release(probe_batch) }
}

pub fn probe_batch_get_num_probes(probe_batch: IPLProbeBatch) -> c_int {
    unsafe { raw_probe_batch_get_num_probes(probe_batch) }
}

pub fn probe_batch_add_probe(probe_batch: IPLProbeBatch, probe: IPLSphere) {
    unsafe { raw_probe_batch_add_probe(probe_batch, probe) }
}

pub fn probe_batch_add_probe_array(probe_batch: IPLProbeBatch, probe_array: IPLProbeArray) {
    unsafe { raw_probe_batch_add_probe_array(probe_batch, probe_array) }
}

pub fn probe_batch_commit(probe_batch: IPLProbeBatch) {
    unsafe { raw_probe_batch_commit(probe_batch) }
}

pub fn probe_batch_get_data_size(
    probe_batch: IPLProbeBatch,
    identifier: &mut IPLBakedDataIdentifier,
) -> usize {
    unsafe { raw_probe_batch_get_data_size(probe_batch, identifier) }
}

pub fn path_baker_bake(context: IPLContext, params: &mut IPLPathBakeParams) -> BakeProgress {
    let state = BakeProgressState {
        callback_count: AtomicU32::new(0),
        final_fraction_bits: AtomicU32::new(0.0_f32.to_bits()),
    };
    unsafe {
        raw_path_baker_bake(
            context,
            params,
            Some(record_bake_progress),
            (&state as *const BakeProgressState).cast_mut().cast(),
        )
    };
    BakeProgress {
        callback_count: state.callback_count.load(Ordering::Relaxed),
        final_fraction: f32::from_bits(state.final_fraction_bits.load(Ordering::Relaxed)),
    }
}

pub fn simulator_create(
    context: IPLContext,
    settings: &mut IPLSimulationSettings,
    simulator: &mut IPLSimulator,
) -> c_int {
    unsafe { raw_simulator_create(context, settings, simulator) }
}

pub fn simulator_release(simulator: &mut IPLSimulator) {
    unsafe { raw_simulator_release(simulator) }
}

pub fn simulator_set_scene(simulator: IPLSimulator, scene: IPLScene) {
    unsafe { raw_simulator_set_scene(simulator, scene) }
}

pub fn simulator_add_probe_batch(simulator: IPLSimulator, probe_batch: IPLProbeBatch) {
    unsafe { raw_simulator_add_probe_batch(simulator, probe_batch) }
}

pub fn simulator_remove_probe_batch(simulator: IPLSimulator, probe_batch: IPLProbeBatch) {
    unsafe { raw_simulator_remove_probe_batch(simulator, probe_batch) }
}

pub fn simulator_set_shared_inputs(
    simulator: IPLSimulator,
    flags: c_int,
    shared_inputs: &mut IPLSimulationSharedInputs,
) {
    unsafe { raw_simulator_set_shared_inputs(simulator, flags, shared_inputs) }
}

pub fn simulator_commit(simulator: IPLSimulator) {
    unsafe { raw_simulator_commit(simulator) }
}

pub fn simulator_run_direct(simulator: IPLSimulator) {
    unsafe { raw_simulator_run_direct(simulator) }
}

pub fn simulator_run_reflections(simulator: IPLSimulator) {
    unsafe { raw_simulator_run_reflections(simulator) }
}

pub fn simulator_run_pathing(simulator: IPLSimulator) {
    unsafe { raw_simulator_run_pathing(simulator) }
}

pub fn source_create(
    simulator: IPLSimulator,
    settings: &mut IPLSourceSettings,
    source: &mut IPLSource,
) -> c_int {
    unsafe { raw_source_create(simulator, settings, source) }
}

pub fn source_release(source: &mut IPLSource) {
    unsafe { raw_source_release(source) }
}

pub fn source_add(source: IPLSource, simulator: IPLSimulator) {
    unsafe { raw_source_add(source, simulator) }
}

pub fn source_remove(source: IPLSource, simulator: IPLSimulator) {
    unsafe { raw_source_remove(source, simulator) }
}

pub fn source_set_inputs(source: IPLSource, flags: c_int, inputs: &mut IPLSimulationInputs) {
    unsafe { raw_source_set_inputs(source, flags, inputs) }
}

pub fn source_get_outputs(source: IPLSource, flags: c_int, outputs: &mut IPLSimulationOutputs) {
    unsafe { raw_source_get_outputs(source, flags, outputs) }
}

pub fn distance_attenuation_calculate(
    context: IPLContext,
    source: IPLVector3,
    listener: IPLVector3,
    model: &mut IPLDistanceAttenuationModel,
) -> f32 {
    unsafe { raw_distance_attenuation_calculate(context, source, listener, model) }
}

pub fn air_absorption_calculate(
    context: IPLContext,
    source: IPLVector3,
    listener: IPLVector3,
    model: &mut IPLAirAbsorptionModel,
) -> [f32; 3] {
    let mut output = [0.0; 3];
    unsafe { raw_air_absorption_calculate(context, source, listener, model, output.as_mut_ptr()) };
    output
}

/// Copies `count` borrowed SDK coefficients before any subsequent simulator call.
pub fn copy_path_coefficients(pointer: *mut f32, count: usize) -> Option<Vec<f32>> {
    if pointer.is_null() || count == 0 {
        return None;
    }
    // SAFETY: `iplSourceGetOutputs` promises this many coefficients for the configured
    // pathing order while the source generation remains alive.
    Some(unsafe { core::slice::from_raw_parts(pointer.cast_const(), count) }.to_vec())
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn audited_v4_8_1_layouts_have_expected_64_bit_sizes() {
        if size_of::<usize>() != 8 {
            return;
        }
        assert_eq!(size_of::<IPLContextSettings>(), 40);
        assert_eq!(size_of::<IPLSceneSettings>(), 64);
        assert_eq!(size_of::<IPLStaticMeshSettings>(), 48);
        assert_eq!(size_of::<IPLAudioBuffer>(), 16);
        assert_eq!(size_of::<IPLHRTFSettings>(), 40);
        assert_eq!(size_of::<IPLPathEffectSettings>(), 32);
        assert_eq!(size_of::<IPLReflectionEffectParams>(), 72);
        assert_eq!(size_of::<IPLSimulationSettings>(), 80);
        assert_eq!(size_of::<IPLSimulationInputs>(), 264);
        assert_eq!(size_of::<IPLSimulationSharedInputs>(), 88);
        assert_eq!(size_of::<IPLSimulationOutputs>(), 216);
        assert_eq!(align_of::<IPLSimulationInputs>(), 8);
    }
}
