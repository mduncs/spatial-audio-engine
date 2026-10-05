#if canImport(FightboxC)
import FightboxC
#endif
import AudioToolbox
import Foundation

/// Construction-time source shape for the neutral spatial route.
public struct FightboxSpatialSourceProgram: Sendable, Equatable {
    public enum Geometry: Sendable, Equatable {
        case point
        case multiPoint(pointCount: UInt8)
        case lineSegment(lengthMeters: Float)
        case stereoImage(widthMeters: Float)
    }

    public var channelCount: UInt32
    public var geometry: Geometry

    public init(channelCount: UInt32, geometry: Geometry) {
        self.channelCount = channelCount
        self.geometry = geometry
    }
}

public enum FightboxSpatialQuality: UInt32, Sendable {
    case desktop = 0
    case mobile = 1
}

public struct FightboxSpatialSourceState: Sendable {
    public var active: Bool
    public var pose: FightboxPose
    public var linearVelocityMPS: SIMD3<Float>

    public init(
        active: Bool,
        pose: FightboxPose,
        linearVelocityMPS: SIMD3<Float> = .zero
    ) {
        self.active = active
        self.pose = pose
        self.linearVelocityMPS = linearVelocityMPS
    }
}

/// Stable, manually allocated source-program storage for one neutral callback.
///
/// The storage is source-major, then channel-major. A producer called from the
/// audio thread may fill channels in place without reallocating the bank.
public final class FightboxSpatialProgramBank: @unchecked Sendable {
    public let sourceCount: Int
    public let blockSizeFrames: Int

    private let programs: [FightboxSpatialSourceProgram]
    private let samples: UnsafeMutablePointer<Float>
    fileprivate let records: UnsafeMutablePointer<FbSourceProgramInputV2>
    private let sampleCapacity: Int

    public init(programs: [FightboxSpatialSourceProgram], blockSizeFrames: Int) {
        precondition(!programs.isEmpty)
        precondition(blockSizeFrames > 0)
        self.programs = programs
        sourceCount = programs.count
        self.blockSizeFrames = blockSizeFrames
        sampleCapacity = programs.count * Int(FB_MAX_PROGRAM_CHANNELS_V2) * blockSizeFrames
        samples = .allocate(capacity: sampleCapacity)
        samples.initialize(repeating: 0, count: sampleCapacity)
        records = .allocate(capacity: programs.count)

        for sourceIndex in programs.indices {
            var record = FbSourceProgramInputV2()
            let channelCount = Int(programs[sourceIndex].channelCount)
            record.source_index = UInt32(sourceIndex)
            record.channel_count = UInt32(channelCount)
            record.samples = UnsafePointer(
                samples.advanced(by: sourceIndex * Int(FB_MAX_PROGRAM_CHANNELS_V2) * blockSizeFrames)
            )
            record.sample_count = channelCount * blockSizeFrames
            record.channel_stride_samples = blockSizeFrames
            records.advanced(by: sourceIndex).initialize(to: record)
        }
    }

    deinit {
        records.deinitialize(count: sourceCount)
        records.deallocate()
        samples.deinitialize(count: sampleCapacity)
        samples.deallocate()
    }

    /// Clears every admitted source plane. This is allocation-free.
    public func clear() {
        samples.update(repeating: 0, count: sampleCapacity)
    }

    /// Borrows one configured program channel for an allocation-free fill.
    public func withMutableChannel<R>(
        sourceIndex: Int,
        channel: Int,
        _ body: (UnsafeMutableBufferPointer<Float>) throws -> R
    ) rethrows -> R {
        precondition(programs.indices.contains(sourceIndex))
        precondition(channel >= 0 && channel < Int(programs[sourceIndex].channelCount))
        let base = samples.advanced(
            by: (sourceIndex * Int(FB_MAX_PROGRAM_CHANNELS_V2) + channel) * blockSizeFrames
        )
        return try body(UnsafeMutableBufferPointer(start: base, count: blockSizeFrames))
    }

    /// Borrows one configured program channel for focused dataflow inspection.
    /// The audio callback should use `withMutableChannel` and avoid retaining
    /// either pointer beyond the closure.
    public func withChannel<R>(
        sourceIndex: Int,
        channel: Int,
        _ body: (UnsafeBufferPointer<Float>) throws -> R
    ) rethrows -> R {
        precondition(programs.indices.contains(sourceIndex))
        precondition(channel >= 0 && channel < Int(programs[sourceIndex].channelCount))
        let base = samples.advanced(
            by: (sourceIndex * Int(FB_MAX_PROGRAM_CHANNELS_V2) + channel) * blockSizeFrames
        )
        return try body(UnsafeBufferPointer(start: base, count: blockSizeFrames))
    }
}

/// Caller-owned fixed-capacity direct/environmental output and metadata bank.
public final class FightboxNeutralSpatialStorage: @unchecked Sendable {
    public let blockSizeFrames: Int

    let direct: UnsafeMutablePointer<Float>
    let environmental: UnsafeMutablePointer<Float>
    let feeds: UnsafeMutablePointer<FbPresentationFeedMetadataV2>
    let metadata: UnsafeMutablePointer<FbSpatialBlockMetadataV2>

    private let directSampleCount: Int
    private let environmentalSampleCount: Int

    public init(blockSizeFrames: Int) {
        precondition(blockSizeFrames > 0)
        self.blockSizeFrames = blockSizeFrames
        directSampleCount = Int(FB_MAX_PRESENTATION_FEEDS_V2) * blockSizeFrames
        environmentalSampleCount = Int(FB_MAX_ENVIRONMENTAL_CHANNELS_V2) * blockSizeFrames

        direct = .allocate(capacity: directSampleCount)
        direct.initialize(repeating: 0, count: directSampleCount)
        environmental = .allocate(capacity: environmentalSampleCount)
        environmental.initialize(repeating: 0, count: environmentalSampleCount)
        feeds = .allocate(capacity: Int(FB_MAX_PRESENTATION_FEEDS_V2))
        feeds.initialize(
            repeating: FbPresentationFeedMetadataV2(),
            count: Int(FB_MAX_PRESENTATION_FEEDS_V2)
        )
        metadata = .allocate(capacity: 1)
        metadata.initialize(to: FbSpatialBlockMetadataV2())
    }

    deinit {
        metadata.deinitialize(count: 1)
        metadata.deallocate()
        feeds.deinitialize(count: Int(FB_MAX_PRESENTATION_FEEDS_V2))
        feeds.deallocate()
        environmental.deinitialize(count: environmentalSampleCount)
        environmental.deallocate()
        direct.deinitialize(count: directSampleCount)
        direct.deallocate()
    }
}

public enum FightboxStableSpatialKeyError: Error, Equatable, Sendable {
    case wrongByteCount(Int)
    case zeroKey
}

/// Exact 128-bit identity emitted by the package authoring pipeline. This is a
/// key only; it never carries a proxy position or authorizes nearest-anchor
/// inference.
public struct FightboxStableSpatialKey: Sendable, Hashable {
    public static let byteCount = 16
    public let bytes: [UInt8]

    public init(bytes: [UInt8]) throws {
        guard bytes.count == Self.byteCount else {
            throw FightboxStableSpatialKeyError.wrongByteCount(bytes.count)
        }
        guard bytes.contains(where: { $0 != 0 }) else {
            throw FightboxStableSpatialKeyError.zeroKey
        }
        self.bytes = bytes
    }
}

public enum FightboxMacroEventRole: UInt32, Sendable {
    case cinematicImpulse = 0
    case standardImpulse = 1
    case ballisticCrack = 2
    case ballisticBlast = 3
}

public enum FightboxMacroAssetTransport: UInt32, Sendable {
    case seekable = 0
    case preGenerated = 1
    case deterministicGenerator = 2
    case nonSeekableLive = 3
}

public struct FightboxMacroEventAdmission: Sendable, Equatable {
    public let eventID: UInt64
    public let atomicGroupID: UInt64
    public let role: FightboxMacroEventRole
    public let assetTransport: FightboxMacroAssetTransport
    public let assetKey: UInt64
    public let emissionFrame: UInt64
    public let programSeekFrame: UInt64
    public let retainedFramesAfterActivation: UInt64
    public let emitterPositionENU: SIMD3<Float>
    public let localHorizonMeters: Float
    public let recordingCarriesMotion: Bool

    public init(
        eventID: UInt64,
        atomicGroupID: UInt64,
        role: FightboxMacroEventRole,
        assetTransport: FightboxMacroAssetTransport,
        assetKey: UInt64,
        emissionFrame: UInt64,
        programSeekFrame: UInt64,
        retainedFramesAfterActivation: UInt64,
        emitterPositionENU: SIMD3<Float>,
        localHorizonMeters: Float = 600,
        recordingCarriesMotion: Bool = false
    ) {
        self.eventID = eventID
        self.atomicGroupID = atomicGroupID
        self.role = role
        self.assetTransport = assetTransport
        self.assetKey = assetKey
        self.emissionFrame = emissionFrame
        self.programSeekFrame = programSeekFrame
        self.retainedFramesAfterActivation = retainedFramesAfterActivation
        self.emitterPositionENU = emitterPositionENU
        self.localHorizonMeters = localHorizonMeters
        self.recordingCarriesMotion = recordingCarriesMotion
    }
}

public struct FightboxMacroDiffuseProfile: Sendable, Equatable {
    public let wetGain: Float
    public let rt60Seconds: Float
    public let highFrequencyDamping: Float

    public init(wetGain: Float, rt60Seconds: Float, highFrequencyDamping: Float) {
        self.wetGain = wetGain
        self.rt60Seconds = rt60Seconds
        self.highFrequencyDamping = highFrequencyDamping
    }
}

public struct FightboxMacroTokenEvent: Sendable, Equatable {
    public let eventID: UInt64
    public let atomicGroupID: UInt64
    public let role: UInt32
    public let assetKey: UInt64
    public let activationFrame: UInt64
    public let programSeekFrame: UInt64
    public let tailDeadlineFrame: UInt64

    public init(
        eventID: UInt64,
        atomicGroupID: UInt64,
        role: UInt32,
        assetKey: UInt64,
        activationFrame: UInt64,
        programSeekFrame: UInt64,
        tailDeadlineFrame: UInt64
    ) {
        self.eventID = eventID
        self.atomicGroupID = atomicGroupID
        self.role = role
        self.assetKey = assetKey
        self.activationFrame = activationFrame
        self.programSeekFrame = programSeekFrame
        self.tailDeadlineFrame = tailDeadlineFrame
    }
}

public struct FightboxMacroTokenPreparation: Sendable, Equatable {
    public let tokenID: UInt64
    public let lookaheadFrame: UInt64
    public let events: [FightboxMacroTokenEvent]

    public init(tokenID: UInt64, lookaheadFrame: UInt64, events: [FightboxMacroTokenEvent]) {
        self.tokenID = tokenID
        self.lookaheadFrame = lookaheadFrame
        self.events = events
    }
}

public struct FightboxMacroPreparedAsset: Sendable, Equatable {
    public let tokenID: UInt64
    public let eventID: UInt64
    public let role: UInt32
    public let readiness: FightboxMacroAssetReadiness

    public init(
        tokenID: UInt64,
        eventID: UInt64,
        role: UInt32,
        readiness: FightboxMacroAssetReadiness
    ) {
        self.tokenID = tokenID
        self.eventID = eventID
        self.role = role
        self.readiness = readiness
    }
}

public struct FightboxMacroCommitResult: Sendable, Equatable {
    public let ffiResultRawValue: Int32
    public let tokenID: UInt64
    public let status: UInt32
    public let eventCount: Int
    public let directGeneration: UInt64
    public let effectiveFrame: UInt64
    /// Maximum exact committed macro/echo tail deadline. ACK completion remains authoritative.
    public let tailDeadlineFrame: UInt64

    public init(
        ffiResultRawValue: Int32,
        tokenID: UInt64,
        status: UInt32,
        eventCount: Int,
        directGeneration: UInt64,
        effectiveFrame: UInt64,
        tailDeadlineFrame: UInt64
    ) {
        self.ffiResultRawValue = ffiResultRawValue
        self.tokenID = tokenID
        self.status = status
        self.eventCount = eventCount
        self.directGeneration = directGeneration
        self.effectiveFrame = effectiveFrame
        self.tailDeadlineFrame = tailDeadlineFrame
    }
}

public struct FightboxMacroAudioAcknowledgement: Sendable, Equatable {
    public let tokenID: UInt64
    public let eventID: UInt64
    public let role: UInt32
    public let status: UInt32
    public let readiness: FightboxMacroAssetReadiness
    public let directGeneration: UInt64
    public let effectiveFrame: UInt64

    public init(
        tokenID: UInt64,
        eventID: UInt64,
        role: UInt32,
        status: UInt32,
        readiness: FightboxMacroAssetReadiness,
        directGeneration: UInt64,
        effectiveFrame: UInt64
    ) {
        self.tokenID = tokenID
        self.eventID = eventID
        self.role = role
        self.status = status
        self.readiness = readiness
        self.directGeneration = directGeneration
        self.effectiveFrame = effectiveFrame
    }
}

/// Preallocated four-request Swift value bank used by the audio callback.
public final class FightboxMacroCallbackStorage: @unchecked Sendable {
    private var requests: [FightboxMacroProgramRequest]
    fileprivate var count = 0

    public init() {
        let emptyReadiness = FightboxMacroAssetReadiness(
            assetKey: 0,
            sourceIndex: 0,
            programSeekFrame: 0,
            discontinuitySequence: 0
        )
        let empty = FightboxMacroProgramRequest(
            readiness: emptyReadiness,
            interval: FightboxMacroProgramInterval(
                assetFrameStart: 0,
                frameCount: 0,
                destinationFrameOffset: 0
            )
        )
        requests = [FightboxMacroProgramRequest](repeating: empty, count: 4)
    }

    fileprivate func replace(index: Int, with request: FightboxMacroProgramRequest) {
        requests[index] = request
    }

    public func withRequests<R>(
        _ body: (UnsafeBufferPointer<FightboxMacroProgramRequest>) throws -> R
    ) rethrows -> R {
        try requests.withUnsafeBufferPointer { buffer in
            let prefix = UnsafeBufferPointer(start: buffer.baseAddress, count: count)
            return try body(prefix)
        }
    }
}

/// Swift owner for the immutable-route V2 neutral spatial session.
///
/// Control calls must be serialized away from the audio callback. `render`
/// is allocation-free after the session, program bank, and output storage have
/// been constructed.
public final class FightboxNeutralSpatialSession: @unchecked Sendable {
    public let sampleRateHz: UInt32
    public let blockSizeFrames: Int
    public let programs: [FightboxSpatialSourceProgram]
    public let environmentalOrder: UInt32
    public let macroProductionBridgeEnabled: Bool

    private let handle: OpaquePointer
    private let controlQueue: DispatchQueue

    public init(
        sampleRateHz: UInt32 = 48_000,
        blockSizeFrames: UInt32 = 128,
        programs: [FightboxSpatialSourceProgram],
        environmentalOrder: UInt32 = 2,
        defaultSourceLevelDB: Float = 0,
        enableMacroProductionBridge: Bool = false,
        macroDiffuseProfile: FightboxMacroDiffuseProfile? = nil,
        quality: FightboxSpatialQuality,
        packageURL: URL,
        bakeURL: URL
    ) throws {
        guard !programs.isEmpty,
              programs.count <= 16,
              (!enableMacroProductionBridge || programs.count == 16),
              (!enableMacroProductionBridge || macroDiffuseProfile != nil),
              environmentalOrder <= UInt32(FB_MAX_ENVIRONMENTAL_ORDER_V2)
        else {
            throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
        }

        let queue = DispatchQueue(
            label: "fightbox.neutral-spatial.control",
            qos: .userInteractive
        )
        var config = FbSessionConfigV2()
        config.abi_version = UInt32(FB_ABI_VERSION_V2)
        config.struct_size = UInt32(MemoryLayout<FbSessionConfigV2>.size)
        config.sample_rate_hz = sampleRateHz
        config.block_size_frames = blockSizeFrames
        config.source_count = UInt32(programs.count)
        config.default_source_level_db = defaultSourceLevelDB
        config.quality_tier = quality.rawValue
        config.render_route = FbRenderNeutralSpatialV2.rawValue
        config.environmental_order = environmentalOrder

        var newHandle: OpaquePointer?
        let result = packageURL.path.withCString { packagePath in
            bakeURL.path.withCString { bakePath in
                fb_session_create_v2(&config, packagePath, bakePath, &newHandle)
            }
        }
        try Self.check(result)
        guard let newHandle else {
            throw FightboxError.ffi(code: Int32(FbInvalidState.rawValue))
        }

        do {
            if enableMacroProductionBridge {
                guard let macroDiffuseProfile else {
                    throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
                }
                var bridgeConfig = FbMacroProductionBridgeConfigV3()
                bridgeConfig.abi_version = UInt32(FB_ABI_VERSION_V3)
                bridgeConfig.struct_size = UInt32(
                    MemoryLayout<FbMacroProductionBridgeConfigV3>.size
                )
                bridgeConfig.diffuse_wet_gain = macroDiffuseProfile.wetGain
                bridgeConfig.diffuse_rt60_s = macroDiffuseProfile.rt60Seconds
                bridgeConfig.diffuse_high_frequency_damping =
                    macroDiffuseProfile.highFrequencyDamping
                try Self.check(
                    fb_session_enable_macro_production_bridge_v3(
                        newHandle,
                        &bridgeConfig
                    )
                )
            }
            for (sourceIndex, program) in programs.enumerated() {
                var sourceConfig = try Self.makeSourceConfig(
                    sourceIndex: sourceIndex,
                    program: program
                )
                try Self.check(fb_session_configure_source_v2(newHandle, &sourceConfig))
            }
        } catch {
            _ = fb_session_destroy(newHandle)
            throw error
        }

        handle = newHandle
        controlQueue = queue
        self.sampleRateHz = sampleRateHz
        self.blockSizeFrames = Int(blockSizeFrames)
        self.programs = programs
        self.environmentalOrder = environmentalOrder
        macroProductionBridgeEnabled = enableMacroProductionBridge
    }

    func prepareCellHandle(
        packageURL: URL,
        bakeURL: URL,
        cityKey: (UInt64, UInt64),
        cellKey: (UInt64, UInt64),
        rawCellBytes: UInt64,
        preparedResidentBytes: UInt64
    ) throws -> OpaquePointer {
        try controlQueue.sync {
            var config = FbCellConfigV2()
            config.abi_version = UInt32(FB_ABI_VERSION_V2)
            config.struct_size = UInt32(MemoryLayout<FbCellConfigV2>.size)
            config.city_key_high = cityKey.0
            config.city_key_low = cityKey.1
            config.cell_key_high = cellKey.0
            config.cell_key_low = cellKey.1
            config.raw_cell_bytes = rawCellBytes
            config.prepared_resident_bytes = preparedResidentBytes
            var prepared: OpaquePointer?
            let result = packageURL.path.withCString { packagePath in
                bakeURL.path.withCString { bakePath in
                    fb_session_prepare_cell_v2(
                        handle,
                        packagePath,
                        bakePath,
                        &config,
                        &prepared
                    )
                }
            }
            try Self.check(result)
            guard let prepared else {
                throw FightboxError.ffi(code: Int32(FbInvalidState.rawValue))
            }
            return prepared
        }
    }

    func offerPreparedCellHandle(_ prepared: OpaquePointer) throws {
        try controlQueue.sync {
            try Self.check(fb_session_offer_prepared_cell_v2(handle, prepared))
        }
    }

    func destroyPreparedCellHandle(_ prepared: OpaquePointer) {
        let result = controlQueue.sync {
            fb_prepared_cell_destroy_v2(prepared)
        }
        assert(
            result.rawValue == FbOk.rawValue,
            "Fightbox prepared cell destroy failed"
        )
    }

    func cellStreamPhase() throws -> UInt32 {
        try controlQueue.sync {
            var state = FbCellStreamStateV2()
            try Self.check(fb_session_cell_stream_state_v2(handle, &state))
            return state.phase
        }
    }

    func collectRetiredCell() throws {
        try controlQueue.sync {
            try Self.check(fb_session_collect_retired_cell_v2(handle))
        }
    }

    deinit {
        let result = controlQueue.sync {
            fb_session_destroy(handle)
        }
        assert(result.rawValue == FbOk.rawValue, "Fightbox neutral session destroy failed")
    }

    public func makeProgramBank() -> FightboxSpatialProgramBank {
        FightboxSpatialProgramBank(programs: programs, blockSizeFrames: blockSizeFrames)
    }

    public func makeRenderStorage() -> FightboxNeutralSpatialStorage {
        FightboxNeutralSpatialStorage(blockSizeFrames: blockSizeFrames)
    }

    public func updateListener(
        pose: FightboxPose,
        linearVelocityMPS: SIMD3<Float> = .zero
    ) throws {
        try controlQueue.sync {
            var ffiPose = Self.makeFFIPose(pose)
            var velocity = Self.makeFFIVector(linearVelocityMPS)
            try Self.check(fb_session_update_listener(handle, &ffiPose, &velocity))
        }
    }

    public func updateSource(
        index: UInt32,
        active: Bool,
        pose: FightboxPose,
        linearVelocityMPS: SIMD3<Float> = .zero
    ) throws {
        try controlQueue.sync {
            var update = FbSourceUpdate()
            update.active = active ? 1 : 0
            update.pose = Self.makeFFIPose(pose)
            update.linear_velocity_mps = Self.makeFFIVector(linearVelocityMPS)
            try Self.check(fb_session_update_source(handle, index, &update))
        }
    }

    /// Publishes one correlated listener/source frame and advances Steam's
    /// control cadence once, regardless of the number of logical sources.
    public func updateControlFrame(
        listenerPose: FightboxPose,
        listenerLinearVelocityMPS: SIMD3<Float> = .zero,
        sources: [FightboxSpatialSourceState]
    ) throws {
        guard sources.count == programs.count else {
            throw FightboxError.invalidBufferCount(
                expected: programs.count,
                actual: sources.count
            )
        }
        try controlQueue.sync {
            let sourceUpdates = sources.map { source in
                var update = FbSourceUpdate()
                update.active = source.active ? 1 : 0
                update.pose = Self.makeFFIPose(source.pose)
                update.linear_velocity_mps = Self.makeFFIVector(
                    source.linearVelocityMPS
                )
                return update
            }
            try sourceUpdates.withUnsafeBufferPointer { updates in
                var frame = FbControlFrameV2()
                frame.abi_version = UInt32(FB_ABI_VERSION_V2)
                frame.struct_size = UInt32(MemoryLayout<FbControlFrameV2>.size)
                frame.source_updates = updates.baseAddress
                frame.source_count = UInt32(updates.count)
                frame.source_update_stride_bytes = UInt32(
                    MemoryLayout<FbSourceUpdate>.stride
                )
                frame.listener_pose = Self.makeFFIPose(listenerPose)
                frame.listener_linear_velocity_mps = Self.makeFFIVector(
                    listenerLinearVelocityMPS
                )
                try Self.check(fb_session_update_control_frame_v2(handle, &frame))
            }
        }
    }

    public func admitMacroEvents(_ events: [FightboxMacroEventAdmission]) throws {
        guard !events.isEmpty else {
            throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
        }
        try controlQueue.sync {
            let records = events.map { event in
                var record = FbMacroEventRequestV2()
                record.abi_version = UInt32(FB_ABI_VERSION_V2)
                record.struct_size = UInt32(MemoryLayout<FbMacroEventRequestV2>.size)
                record.event_id = event.eventID
                record.atomic_group_id = event.atomicGroupID
                record.role = event.role.rawValue
                record.asset_transport = event.assetTransport.rawValue
                record.asset_key = event.assetKey
                record.emission_frame = event.emissionFrame
                record.program_seek_frame = event.programSeekFrame
                record.retained_frames_after_activation = event.retainedFramesAfterActivation
                record.emitter_position_enu = Self.makeFFIVector(event.emitterPositionENU)
                record.local_horizon_m = event.localHorizonMeters
                record.recording_carries_motion = event.recordingCarriesMotion ? 1 : 0
                return record
            }
            try records.withUnsafeBufferPointer { buffer in
                try Self.check(
                    fb_session_admit_macro_event_group_v2(
                        handle,
                        buffer.baseAddress,
                        UInt32(buffer.count)
                    )
                )
            }
        }
    }

    /// Explicitly binds an admitted dormant event to a real package-authored
    /// static ingress anchor. Generic remote events remain echo-off.
    public func bindMacroEchoAnchor(
        eventID: UInt64,
        anchorKey: FightboxStableSpatialKey
    ) throws {
        try controlQueue.sync {
            var binding = FbMacroEchoAnchorBindingV3()
            binding.abi_version = UInt32(FB_ABI_VERSION_V3)
            binding.struct_size = UInt32(MemoryLayout<FbMacroEchoAnchorBindingV3>.size)
            binding.event_id = eventID
            withUnsafeMutableBytes(of: &binding.anchor_key) { destination in
                anchorKey.bytes.withUnsafeBytes { source in
                    destination.copyBytes(from: source)
                }
            }
            try Self.check(fb_session_bind_macro_echo_anchor_v3(handle, &binding))
        }
    }

    public func prepareMacroToken(
        lookaheadFrame: UInt64
    ) throws -> FightboxMacroTokenPreparation? {
        guard macroProductionBridgeEnabled else {
            throw FightboxError.ffi(code: Int32(FbInvalidState.rawValue))
        }
        return try controlQueue.sync {
            var batch = FbMacroPrepareBatchV3()
            batch.abi_version = UInt32(FB_ABI_VERSION_V3)
            batch.struct_size = UInt32(MemoryLayout<FbMacroPrepareBatchV3>.size)
            try Self.check(fb_session_prepare_macro_token_v3(handle, lookaheadFrame, &batch))
            let count = Int(batch.event_count)
            guard count >= 0, count <= Int(FB_MAX_MACRO_TOKEN_EVENTS_V3) else {
                throw FightboxError.ffi(code: Int32(FbInvalidState.rawValue))
            }
            if count == 0 { return nil }
            guard batch.token_id != 0 else {
                throw FightboxError.ffi(code: Int32(FbInvalidState.rawValue))
            }
            let events = withUnsafePointer(to: &batch.events) { tuple in
                tuple.withMemoryRebound(
                    to: FbMacroPrepareEventV3.self,
                    capacity: Int(FB_MAX_MACRO_TOKEN_EVENTS_V3)
                ) { records in
                    (0 ..< count).map { index in
                        let record = records[index]
                        return FightboxMacroTokenEvent(
                            eventID: record.event_id,
                            atomicGroupID: record.atomic_group_id,
                            role: record.role,
                            assetKey: record.asset_key,
                            activationFrame: record.activation_frame,
                            programSeekFrame: record.program_seek_frame,
                            tailDeadlineFrame: record.tail_deadline_frame
                        )
                    }
                }
            }
            return FightboxMacroTokenPreparation(
                tokenID: batch.token_id,
                lookaheadFrame: batch.lookahead_frame,
                events: events
            )
        }
    }

    public func stageMacroReady(_ assets: [FightboxMacroPreparedAsset]) throws {
        guard macroProductionBridgeEnabled,
              !assets.isEmpty,
              assets.count <= Int(FB_MAX_MACRO_TOKEN_EVENTS_V3)
        else {
            throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
        }
        try controlQueue.sync {
            let records = assets.map { asset in
                var record = FbMacroReadyAssetV3()
                record.abi_version = UInt32(FB_ABI_VERSION_V3)
                record.struct_size = UInt32(MemoryLayout<FbMacroReadyAssetV3>.size)
                record.token_id = asset.tokenID
                record.event_id = asset.eventID
                record.role = asset.role
                record.source_index = UInt32(asset.readiness.sourceIndex)
                record.asset_key = asset.readiness.assetKey
                record.program_seek_frame = asset.readiness.programSeekFrame
                record.discontinuity_sequence = asset.readiness.discontinuitySequence
                return record
            }
            try records.withUnsafeBufferPointer { buffer in
                try Self.check(
                    fb_session_stage_macro_ready_v3(
                        handle,
                        buffer.baseAddress,
                        UInt32(buffer.count)
                    )
                )
            }
        }
    }

    public func commitMacroControlFrame(
        tokenID: UInt64,
        listenerPose: FightboxPose,
        listenerLinearVelocityMPS: SIMD3<Float> = .zero,
        sources: [FightboxSpatialSourceState]
    ) throws -> FightboxMacroCommitResult {
        guard macroProductionBridgeEnabled, sources.count == programs.count else {
            throw FightboxError.invalidBufferCount(
                expected: programs.count,
                actual: sources.count
            )
        }
        return try controlQueue.sync {
            let sourceUpdates = sources.map { source in
                var update = FbSourceUpdate()
                update.active = source.active ? 1 : 0
                update.pose = Self.makeFFIPose(source.pose)
                update.linear_velocity_mps = Self.makeFFIVector(source.linearVelocityMPS)
                return update
            }
            return try sourceUpdates.withUnsafeBufferPointer { updates in
                var frame = FbControlFrameV2()
                frame.abi_version = UInt32(FB_ABI_VERSION_V2)
                frame.struct_size = UInt32(MemoryLayout<FbControlFrameV2>.size)
                frame.source_updates = updates.baseAddress
                frame.source_count = UInt32(updates.count)
                frame.source_update_stride_bytes = UInt32(MemoryLayout<FbSourceUpdate>.stride)
                frame.listener_pose = Self.makeFFIPose(listenerPose)
                frame.listener_linear_velocity_mps = Self.makeFFIVector(
                    listenerLinearVelocityMPS
                )
                var result = FbMacroCommitResultV3()
                result.abi_version = UInt32(FB_ABI_VERSION_V3)
                result.struct_size = UInt32(MemoryLayout<FbMacroCommitResultV3>.size)
                let ffi = fb_session_update_control_frame_macro_v3(
                    handle,
                    &frame,
                    tokenID,
                    &result
                )
                // A direct-success transaction may report a later optional
                // path/reflection backend error while remaining committed.
                if ffi.rawValue != FbOk.rawValue, result.status != 3 {
                    try Self.check(ffi)
                }
                return FightboxMacroCommitResult(
                    ffiResultRawValue: Int32(ffi.rawValue),
                    tokenID: result.token_id,
                    status: result.status,
                    eventCount: Int(result.event_count),
                    directGeneration: result.direct_generation,
                    effectiveFrame: result.effective_frame,
                    tailDeadlineFrame: result.tail_deadline_frame
                )
            }
        }
    }

    public func discardMacroToken(_ tokenID: UInt64) throws {
        try controlQueue.sync {
            try Self.check(fb_session_discard_macro_token_v3(handle, tokenID))
        }
    }

    public func pollMacroAcknowledgement() throws -> FightboxMacroAudioAcknowledgement? {
        guard macroProductionBridgeEnabled else { return nil }
        return try controlQueue.sync {
            var ack = FbMacroAudioAckV3()
            ack.abi_version = UInt32(FB_ABI_VERSION_V3)
            ack.struct_size = UInt32(MemoryLayout<FbMacroAudioAckV3>.size)
            let result = fb_session_poll_macro_ack_v3(handle, &ack)
            if result.rawValue == FbInvalidState.rawValue { return nil }
            try Self.check(result)
            return FightboxMacroAudioAcknowledgement(
                tokenID: ack.token_id,
                eventID: ack.event_id,
                role: ack.role,
                status: ack.status,
                readiness: FightboxMacroAssetReadiness(
                    assetKey: ack.asset_key,
                    sourceIndex: Int(ack.source_index),
                    programSeekFrame: ack.program_seek_frame,
                    discontinuitySequence: ack.discontinuity_sequence
                ),
                directGeneration: ack.direct_generation,
                effectiveFrame: ack.effective_frame
            )
        }
    }

    public func finalizeMacroAcknowledgement(
        _ acknowledgement: FightboxMacroAudioAcknowledgement
    ) throws {
        try controlQueue.sync {
            var ack = FbMacroAudioAckV3()
            ack.abi_version = UInt32(FB_ABI_VERSION_V3)
            ack.struct_size = UInt32(MemoryLayout<FbMacroAudioAckV3>.size)
            ack.token_id = acknowledgement.tokenID
            ack.event_id = acknowledgement.eventID
            ack.role = acknowledgement.role
            ack.status = acknowledgement.status
            ack.asset_key = acknowledgement.readiness.assetKey
            ack.source_index = UInt32(acknowledgement.readiness.sourceIndex)
            ack.discontinuity_sequence = acknowledgement.readiness.discontinuitySequence
            ack.direct_generation = acknowledgement.directGeneration
            ack.effective_frame = acknowledgement.effectiveFrame
            ack.program_seek_frame = acknowledgement.readiness.programSeekFrame
            try Self.check(fb_session_finalize_macro_ack_v3(handle, &ack))
        }
    }

    /// Audio-thread begin. The storage is preallocated and receives at most
    /// four exact provider intervals without retaining a C pointer.
    func beginMacroRender(into storage: FightboxMacroCallbackStorage) -> OSStatus {
        guard macroProductionBridgeEnabled else {
            storage.count = 0
            return noErr
        }
        var batch = FbMacroProgramRequestBatchV3()
        batch.abi_version = UInt32(FB_ABI_VERSION_V3)
        batch.struct_size = UInt32(MemoryLayout<FbMacroProgramRequestBatchV3>.size)
        let result = fb_session_macro_render_begin_v3(handle, &batch)
        guard result.rawValue == FbOk.rawValue else {
            storage.count = 0
            return kAudio_ParamError
        }
        let count = Int(batch.request_count)
        guard count >= 0, count <= Int(FB_MAX_MACRO_PROGRAM_REQUESTS_V3) else {
            storage.count = 0
            _ = fb_session_macro_render_end_v3(handle, 0)
            return kAudio_ParamError
        }
        var valid = true
        withUnsafePointer(to: &batch.requests) { tuple in
            tuple.withMemoryRebound(
                to: FbMacroProgramRequestV3.self,
                capacity: Int(FB_MAX_MACRO_PROGRAM_REQUESTS_V3)
            ) { records in
                for index in 0 ..< count {
                    let request = records[index]
                    guard request.abi_version == UInt32(FB_ABI_VERSION_V3),
                          request.struct_size >= UInt32(MemoryLayout<FbMacroProgramRequestV3>.size),
                          request.token_id == batch.token_id,
                          (12 ... 15).contains(Int(request.source_index)),
                          request.discontinuity_sequence != 0,
                          request.discontinuity_sequence & 1 == 0,
                          request.frame_count > 0,
                          Int(request.destination_frame_offset) + Int(request.frame_count) <=
                              blockSizeFrames
                    else {
                        valid = false
                        break
                    }
                    storage.replace(
                        index: index,
                        with: FightboxMacroProgramRequest(
                            readiness: FightboxMacroAssetReadiness(
                                assetKey: request.asset_key,
                                sourceIndex: Int(request.source_index),
                                programSeekFrame: request.program_seek_frame,
                                discontinuitySequence: request.discontinuity_sequence
                            ),
                            interval: FightboxMacroProgramInterval(
                                assetFrameStart: request.asset_frame_start,
                                frameCount: Int(request.frame_count),
                                destinationFrameOffset: Int(request.destination_frame_offset)
                            )
                        )
                    )
                }
            }
        }
        guard valid else {
            storage.count = 0
            _ = fb_session_macro_render_end_v3(handle, 0)
            return kAudio_ParamError
        }
        storage.count = count
        return noErr
    }

    /// Audio-thread end. `commit` is legal only after one successful spatial
    /// render of the begun engine frame.
    func endMacroRender(commit: Bool) -> OSStatus {
        guard macroProductionBridgeEnabled else { return noErr }
        let result = fb_session_macro_render_end_v3(handle, commit ? 1 : 0)
        return result.rawValue == FbOk.rawValue ? noErr : kAudio_ParamError
    }

    /// Publishes the mandatory initial propagation snapshot without consuming
    /// a public render block.
    public func prepare() throws {
        try controlQueue.sync {
            try Self.check(fb_session_prepare_spatial_v2(handle))
        }
    }

    public func render(
        programs input: FightboxSpatialProgramBank,
        into output: FightboxNeutralSpatialStorage
    ) throws {
        guard input.sourceCount == programs.count,
              input.blockSizeFrames == blockSizeFrames,
              output.blockSizeFrames == blockSizeFrames
        else {
            throw FightboxError.invalidBufferCount(
                expected: blockSizeFrames,
                actual: output.blockSizeFrames
            )
        }

        var block = FbSpatialRenderBlockV2()
        block.abi_version = UInt32(FB_ABI_VERSION_V2)
        block.struct_size = UInt32(MemoryLayout<FbSpatialRenderBlockV2>.size)
        block.source_programs = UnsafePointer(input.records)
        block.source_program_count = UInt32(programs.count)
        block.source_program_stride_bytes = UInt32(MemoryLayout<FbSourceProgramInputV2>.stride)
        block.direct_output.samples = output.direct
        block.direct_output.sample_capacity = Int(FB_MAX_PRESENTATION_FEEDS_V2) * blockSizeFrames
        block.direct_output.plane_capacity = UInt32(FB_MAX_PRESENTATION_FEEDS_V2)
        block.direct_output.plane_stride_samples = blockSizeFrames
        block.environmental_output.samples = output.environmental
        block.environmental_output.sample_capacity =
            Int(FB_MAX_ENVIRONMENTAL_CHANNELS_V2) * blockSizeFrames
        block.environmental_output.plane_capacity = UInt32(FB_MAX_ENVIRONMENTAL_CHANNELS_V2)
        block.environmental_output.plane_stride_samples = blockSizeFrames
        block.feed_metadata = output.feeds
        block.feed_metadata_capacity = UInt32(FB_MAX_PRESENTATION_FEEDS_V2)
        block.feed_metadata_stride_bytes = UInt32(MemoryLayout<FbPresentationFeedMetadataV2>.stride)
        block.block_metadata = output.metadata
        try Self.check(fb_session_render_spatial_v2(handle, &block))
    }

    private static func makeSourceConfig(
        sourceIndex: Int,
        program: FightboxSpatialSourceProgram
    ) throws -> FbSourceProgramConfigV2 {
        guard program.channelCount == 1 || program.channelCount == 2 else {
            throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
        }

        var config = FbSourceProgramConfigV2()
        config.abi_version = UInt32(FB_ABI_VERSION_V2)
        config.struct_size = UInt32(MemoryLayout<FbSourceProgramConfigV2>.size)
        config.source_index = UInt32(sourceIndex)
        config.channel_count = program.channelCount

        switch program.geometry {
        case .point:
            guard program.channelCount == 1 else {
                throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
            }
            config.source_geometry = FbSourceGeometryPointV2.rawValue
        case let .multiPoint(pointCount):
            guard program.channelCount == 1, pointCount > 0 else {
                throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
            }
            config.source_geometry = FbSourceGeometryMultiPointV2.rawValue
            config.multipoint_count = UInt32(pointCount)
            config.extent_m = Float(FB_MULTIPOINT_FIXED_EXTENT_METERS_V2)
        case let .lineSegment(lengthMeters):
            guard program.channelCount == 1, lengthMeters.isFinite, lengthMeters > 0 else {
                throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
            }
            config.source_geometry = FbSourceGeometryLineSegmentV2.rawValue
            config.extent_m = lengthMeters
        case let .stereoImage(widthMeters):
            guard widthMeters.isFinite, widthMeters > 0 else {
                throw FightboxError.ffi(code: Int32(FbInvalidArgument.rawValue))
            }
            config.source_geometry = FbSourceGeometryStereoImageV2.rawValue
            config.extent_m = widthMeters
        }
        return config
    }

    private static func check(_ result: FbResult) throws {
        guard result.rawValue == FbOk.rawValue else {
            throw FightboxError.ffi(code: Int32(result.rawValue))
        }
    }

    private static func makeFFIPose(_ pose: FightboxPose) -> FbPose {
        var value = FbPose()
        value.position = makeFFIVector(pose.position)
        value.forward = makeFFIVector(pose.forward)
        value.up = makeFFIVector(pose.up)
        return value
    }

    private static func makeFFIVector(_ vector: SIMD3<Float>) -> FbVec3 {
        var value = FbVec3()
        value.east_m = vector.x
        value.north_m = vector.y
        value.up_m = vector.z
        return value
    }
}
