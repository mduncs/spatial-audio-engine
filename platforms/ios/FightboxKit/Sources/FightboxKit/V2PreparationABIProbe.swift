import FightboxC

// Compile-only coverage for the additive V2 control-thread boundary. The
// production FightboxSession remains a frozen V1 consumer.
enum V2ControlBoundaryABIProbe {
    static func updateControlFrame(
        _ handle: OpaquePointer,
        listenerPose: FbPose,
        listenerLinearVelocityMPS: FbVec3,
        sourceUpdates: [FbSourceUpdate]
    ) -> FbResult {
        sourceUpdates.withUnsafeBufferPointer { updates in
            var frame = FbControlFrameV2()
            frame.abi_version = UInt32(FB_ABI_VERSION_V2)
            frame.struct_size = UInt32(MemoryLayout<FbControlFrameV2>.size)
            frame.source_updates = updates.baseAddress
            frame.source_count = UInt32(updates.count)
            frame.source_update_stride_bytes = UInt32(MemoryLayout<FbSourceUpdate>.stride)
            frame.listener_pose = listenerPose
            frame.listener_linear_velocity_mps = listenerLinearVelocityMPS
            return fb_session_update_control_frame_v2(handle, &frame)
        }
    }

    static func prepare(_ handle: OpaquePointer) -> FbResult {
        fb_session_prepare_spatial_v2(handle)
    }
}
