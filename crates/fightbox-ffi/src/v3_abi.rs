//! Additive tokened macro-production bridge layouts.
//!
//! V2 activation/control structures remain frozen. V3 carries scalar token,
//! readiness, and exact callback interval identities only; no Rust/Swift-owned
//! object or pointer-to-object crosses this seam.

pub const FB_ABI_VERSION_V3: u32 = 3;
pub const FB_MAX_MACRO_TOKEN_EVENTS_V3: u32 = 4;
pub const FB_MAX_MACRO_PROGRAM_REQUESTS_V3: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroEchoAnchorBindingV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub event_id: u64,
    pub anchor_key: [u8; 16],
    pub reserved: [u64; 4],
}

impl Default for FbMacroEchoAnchorBindingV3 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: core::mem::size_of::<Self>() as u32,
            event_id: 0,
            anchor_key: [0; 16],
            reserved: [0; 4],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroProductionBridgeConfigV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub diffuse_wet_gain: f32,
    pub diffuse_rt60_s: f32,
    pub diffuse_high_frequency_damping: f32,
    pub reserved_f32: f32,
    pub reserved: [u64; 4],
}

impl Default for FbMacroProductionBridgeConfigV3 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: core::mem::size_of::<Self>() as u32,
            diffuse_wet_gain: 0.0,
            diffuse_rt60_s: 0.8,
            diffuse_high_frequency_damping: 0.5,
            reserved_f32: 0.0,
            reserved: [0; 4],
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FbMacroTokenStatusV3 {
    #[default]
    FbMacroTokenNoneV3 = 0,
    FbMacroTokenReservedV3 = 1,
    FbMacroTokenReadyV3 = 2,
    FbMacroTokenCommittedV3 = 3,
    FbMacroTokenAudioActiveV3 = 4,
    FbMacroTokenAudioAckedV3 = 5,
    FbMacroTokenTerminalRejectedV3 = 6,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbMacroPrepareEventV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub event_id: u64,
    pub atomic_group_id: u64,
    pub role: u32,
    pub reserved_u32: u32,
    pub asset_key: u64,
    pub activation_frame: u64,
    pub program_seek_frame: u64,
    pub tail_deadline_frame: u64,
    pub reserved: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroPrepareBatchV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub token_id: u64,
    pub lookahead_frame: u64,
    pub event_count: u32,
    pub status: u32,
    pub events: [FbMacroPrepareEventV3; FB_MAX_MACRO_TOKEN_EVENTS_V3 as usize],
    pub reserved: [u64; 2],
}

impl Default for FbMacroPrepareBatchV3 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: core::mem::size_of::<Self>() as u32,
            token_id: 0,
            lookahead_frame: 0,
            event_count: 0,
            status: FbMacroTokenStatusV3::FbMacroTokenNoneV3 as u32,
            events: [FbMacroPrepareEventV3::default(); FB_MAX_MACRO_TOKEN_EVENTS_V3 as usize],
            reserved: [0; 2],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbMacroReadyAssetV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub token_id: u64,
    pub event_id: u64,
    pub role: u32,
    pub source_index: u32,
    pub asset_key: u64,
    pub program_seek_frame: u64,
    pub discontinuity_sequence: u64,
    pub reserved: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroCommitResultV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub token_id: u64,
    pub status: u32,
    pub event_count: u32,
    pub direct_generation: u64,
    pub effective_frame: u64,
    pub tail_deadline_frame: u64,
    pub reserved: [u64; 2],
}

impl Default for FbMacroCommitResultV3 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: core::mem::size_of::<Self>() as u32,
            token_id: 0,
            status: FbMacroTokenStatusV3::FbMacroTokenNoneV3 as u32,
            event_count: 0,
            direct_generation: 0,
            effective_frame: 0,
            tail_deadline_frame: 0,
            reserved: [0; 2],
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FbMacroProgramModeV3 {
    #[default]
    FbMacroProgramDetailedLocalV3 = 0,
    FbMacroProgramFallbackPointV3 = 1,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FbMacroRenderDispositionV3 {
    #[default]
    FbMacroRenderDiscardV3 = 0,
    FbMacroRenderCommitV3 = 1,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FbMacroAudioAckStatusV3 {
    #[default]
    FbMacroAudioCompletedV3 = 0,
    FbMacroAudioTerminalRejectedV3 = 1,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbMacroProgramRequestV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub token_id: u64,
    pub event_id: u64,
    pub asset_key: u64,
    pub role: u32,
    pub mode: u32,
    pub source_index: u32,
    pub reserved_u32: u32,
    pub program_seek_frame: u64,
    pub discontinuity_sequence: u64,
    pub asset_frame_start: u64,
    pub frame_count: u32,
    pub destination_frame_offset: u32,
    pub block_start_frame: u64,
    pub reserved: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroProgramRequestBatchV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub token_id: u64,
    pub block_start_frame: u64,
    pub request_count: u32,
    pub status: u32,
    pub requests: [FbMacroProgramRequestV3; FB_MAX_MACRO_PROGRAM_REQUESTS_V3 as usize],
    pub reserved: [u64; 2],
}

impl Default for FbMacroProgramRequestBatchV3 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: core::mem::size_of::<Self>() as u32,
            token_id: 0,
            block_start_frame: 0,
            request_count: 0,
            status: FbMacroTokenStatusV3::FbMacroTokenNoneV3 as u32,
            requests: [FbMacroProgramRequestV3::default();
                FB_MAX_MACRO_PROGRAM_REQUESTS_V3 as usize],
            reserved: [0; 2],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroAudioAckV3 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub token_id: u64,
    pub event_id: u64,
    pub role: u32,
    pub status: u32,
    pub asset_key: u64,
    pub source_index: u32,
    pub reserved_u32: u32,
    pub discontinuity_sequence: u64,
    pub direct_generation: u64,
    pub effective_frame: u64,
    pub program_seek_frame: u64,
    pub reserved: [u64; 1],
}

impl Default for FbMacroAudioAckV3 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: core::mem::size_of::<Self>() as u32,
            token_id: 0,
            event_id: 0,
            role: 0,
            status: FbMacroAudioAckStatusV3::FbMacroAudioCompletedV3 as u32,
            asset_key: 0,
            source_index: 0,
            reserved_u32: 0,
            discontinuity_sequence: 0,
            direct_generation: 0,
            effective_frame: 0,
            program_seek_frame: 0,
            reserved: [0; 1],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_token_layouts_are_fixed_copy_scalar_records() {
        assert!(!core::mem::needs_drop::<FbMacroPrepareBatchV3>());
        assert!(!core::mem::needs_drop::<FbMacroReadyAssetV3>());
        assert!(!core::mem::needs_drop::<FbMacroCommitResultV3>());
        assert!(!core::mem::needs_drop::<FbMacroProgramRequestBatchV3>());
        assert!(!core::mem::needs_drop::<FbMacroAudioAckV3>());
        assert_eq!(core::mem::size_of::<FbMacroEchoAnchorBindingV3>(), 64);
        assert_eq!(core::mem::size_of::<FbMacroProductionBridgeConfigV3>(), 56);
        assert_eq!(core::mem::size_of::<FbMacroPrepareEventV3>(), 80);
        assert_eq!(core::mem::size_of::<FbMacroPrepareBatchV3>(), 368);
        assert_eq!(core::mem::size_of::<FbMacroReadyAssetV3>(), 72);
        assert_eq!(core::mem::size_of::<FbMacroCommitResultV3>(), 64);
        assert_eq!(core::mem::size_of::<FbMacroProgramRequestV3>(), 104);
        assert_eq!(core::mem::size_of::<FbMacroProgramRequestBatchV3>(), 464);
        assert_eq!(core::mem::size_of::<FbMacroAudioAckV3>(), 88);
        assert_eq!(
            core::mem::offset_of!(FbMacroProgramRequestV3, program_seek_frame),
            48
        );
        assert_eq!(
            core::mem::offset_of!(FbMacroProgramRequestV3, block_start_frame),
            80
        );
        assert_eq!(
            core::mem::offset_of!(FbMacroAudioAckV3, direct_generation),
            56
        );
        assert_eq!(
            core::mem::offset_of!(FbMacroAudioAckV3, program_seek_frame),
            72
        );
        assert_eq!(FbMacroPrepareBatchV3::default().abi_version, 3);
        assert_eq!(FbMacroProgramRequestBatchV3::default().request_count, 0);
    }
}
