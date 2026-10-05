#if os(iOS)
import AudioToolbox
import AVFoundation
#if canImport(FightboxC)
import FightboxC
#endif
import Foundation
import simd

/// A device-qualified map from Fightbox ENU directions to Apple's point-source
/// azimuth/elevation convention.
///
/// The topology spike did not establish Apple's target-device cardinal signs.
/// The provisional map is useful for compile and lab work, but cannot promote
/// the Apple route.
public struct AppleSpatialAxisCalibration: Sendable, Equatable {
    public enum EnvironmentalBasis: UInt32, Sendable, Equatable {
        case rightHandedENU = 0
        case steamXRightYUpZBack = 1
    }

    public var rightENU: SIMD3<Float>
    public var frontENU: SIMD3<Float>
    public var upENU: SIMD3<Float>
    /// Row-major ACN matrix from the declared Fightbox environmental basis to
    /// Apple's device-qualified ACN basis. It is a coordinate-basis transform,
    /// not listener/head rotation.
    public var environmentalAcnTransform: [Float]
    public var expectedEnvironmentalBasis: EnvironmentalBasis
    public var evidenceIdentifier: String
    public var isTargetDeviceVerified: Bool

    public init(
        rightENU: SIMD3<Float>,
        frontENU: SIMD3<Float>,
        upENU: SIMD3<Float>,
        environmentalAcnTransform: [Float],
        expectedEnvironmentalBasis: EnvironmentalBasis,
        evidenceIdentifier: String,
        isTargetDeviceVerified: Bool
    ) {
        precondition(environmentalAcnTransform.count == 81)
        precondition(environmentalAcnTransform.allSatisfy(\.isFinite))
        self.rightENU = rightENU
        self.frontENU = frontENU
        self.upENU = upENU
        self.environmentalAcnTransform = environmentalAcnTransform
        self.expectedEnvironmentalBasis = expectedEnvironmentalBasis
        self.evidenceIdentifier = evidenceIdentifier
        self.isTargetDeviceVerified = isTargetDeviceVerified
    }

    public static let provisionalENU = AppleSpatialAxisCalibration(
        rightENU: SIMD3<Float>(1, 0, 0),
        frontENU: SIMD3<Float>(0, 1, 0),
        upENU: SIMD3<Float>(0, 0, 1),
        environmentalAcnTransform: (0 ..< 81).map { index in
            index / 9 == index % 9 ? 1 : 0
        },
        expectedEnvironmentalBasis: .steamXRightYUpZBack,
        evidenceIdentifier: "compile-only-provisional-enu",
        isTargetDeviceVerified: false
    )
}

public enum AppleSpatialFeatureState: Sendable, Equatable {
    case unavailable(String)
    case requestedUnverified
    case active
}

public struct AppleSpatialRouteStatus: Sendable, Equatable {
    public var isRunning: Bool
    public var mixerLatencyFrames: Int
    public var automaticHeadTracking: AppleSpatialFeatureState
    public var personalizedHRTF: AppleSpatialFeatureState
    public var axisCalibrationIdentifier: String
    public var axisCalibrationVerified: Bool

    public init(
        isRunning: Bool,
        mixerLatencyFrames: Int,
        automaticHeadTracking: AppleSpatialFeatureState,
        personalizedHRTF: AppleSpatialFeatureState,
        axisCalibrationIdentifier: String,
        axisCalibrationVerified: Bool
    ) {
        self.isRunning = isRunning
        self.mixerLatencyFrames = mixerLatencyFrames
        self.automaticHeadTracking = automaticHeadTracking
        self.personalizedHRTF = personalizedHRTF
        self.axisCalibrationIdentifier = axisCalibrationIdentifier
        self.axisCalibrationVerified = axisCalibrationVerified
    }
}

public struct AppleSpatialRouteRequirements: Sendable, Equatable {
    public var requireTargetDeviceAxisCalibration: Bool
    public var requirePersonalizedHRTFActive: Bool

    public init(
        requireTargetDeviceAxisCalibration: Bool = true,
        requirePersonalizedHRTFActive: Bool = true
    ) {
        self.requireTargetDeviceAxisCalibration = requireTargetDeviceAxisCalibration
        self.requirePersonalizedHRTFActive = requirePersonalizedHRTFActive
    }

    /// Compile/lab only. This must never be used as evidence for promotion.
    public static let experimental = AppleSpatialRouteRequirements(
        requireTargetDeviceAxisCalibration: false,
        requirePersonalizedHRTFActive: false
    )
}

public enum AppleSpatialMixerError: Error, Sendable, CustomStringConvertible {
    case requiresIOS18
    case incompatibleOutputRoute(String)
    case unverifiedAxisCalibration
    case personalizedHRTFInactive
    case audioUnit(operation: String, status: OSStatus)

    public var description: String {
        switch self {
        case .requiresIOS18:
            return "Apple spatial output requires iOS 18 or later"
        case let .incompatibleOutputRoute(route):
            return "Apple spatial output requires a headphone route, received \(route)"
        case .unverifiedAxisCalibration:
            return "Apple cardinal-axis calibration has not passed on the target device"
        case .personalizedHRTFInactive:
            return "Apple reported that no mixer input is using the personalized HRTF"
        case let .audioUnit(operation, status):
            return "\(operation) failed with OSStatus \(status)"
        }
    }
}

private let appleSpatialInputCallback: AURenderCallback = {
    reference, flags, timestamp, bus, frameCount, ioData in
    guard let ioData else {
        return kAudio_ParamError
    }
    let renderer = Unmanaged<AppleSpatialMixerRenderer>
        .fromOpaque(reference)
        .takeUnretainedValue()
    return renderer.renderInput(
        flags: flags,
        timestamp: timestamp,
        bus: bus,
        frameCount: frameCount,
        ioData: ioData
    )
}

private final class AppleSpatialDelayBank {
    private let planeCount: Int
    private let ringCapacity: Int
    private let samples: UnsafeMutablePointer<Float>
    private var writeHeads: [Int]

    init(planeCount: Int, maximumDelayFrames: Int) {
        self.planeCount = planeCount
        ringCapacity = maximumDelayFrames + 1
        let count = planeCount * ringCapacity
        samples = .allocate(capacity: count)
        samples.initialize(repeating: 0, count: count)
        writeHeads = [Int](repeating: 0, count: planeCount)
    }

    deinit {
        let count = planeCount * ringCapacity
        samples.deinitialize(count: count)
        samples.deallocate()
    }

    func reset() {
        samples.update(repeating: 0, count: planeCount * ringCapacity)
        for index in writeHeads.indices {
            writeHeads[index] = 0
        }
    }

    func reset(plane: Int) {
        guard plane >= 0, plane < planeCount else { return }
        samples
            .advanced(by: plane * ringCapacity)
            .update(repeating: 0, count: ringCapacity)
        writeHeads[plane] = 0
    }

    func process(
        plane: Int,
        input: UnsafePointer<Float>,
        output: UnsafeMutablePointer<Float>,
        count: Int,
        delayFrames: Int,
        gain: Float = 1
    ) -> Bool {
        guard plane >= 0,
              plane < planeCount,
              delayFrames >= 0,
              delayFrames < ringCapacity
        else {
            return false
        }

        var head = writeHeads[plane]
        let ring = samples.advanced(by: plane * ringCapacity)
        for frame in 0 ..< count {
            let value = input[frame] * gain
            if delayFrames == 0 {
                output[frame] = value
            } else {
                var read = head - delayFrames
                if read < 0 { read += ringCapacity }
                output[frame] = ring[read]
            }
            ring[head] = value
            head += 1
            if head == ringCapacity { head = 0 }
        }
        writeHeads[plane] = head
        return true
    }
}

private final class AppleSpatialMixerRenderer: @unchecked Sendable {
    static let directPlaneCount = Int(FB_MAX_PRESENTATION_FEEDS_V2)
    static let environmentalPlaneCount = Int(FB_MAX_ENVIRONMENTAL_CHANNELS_V2)
    static let environmentalBus = UInt32(FB_MAX_PRESENTATION_FEEDS_V2)
    static let maximumAlignmentFrames = 16_384

    let session: FightboxNeutralSpatialSession
    let programBank: FightboxSpatialProgramBank
    let renderStorage: FightboxNeutralSpatialStorage

    private let provider: FightboxSpatialProgramProvider
    private let transactionalProvider: FightboxTransactionalSpatialProgramProvider?
    private let macroProvider: FightboxCanonicalProgramProvider?
    private let macroCallbackStorage: FightboxMacroCallbackStorage?
    private let calibration: AppleSpatialAxisCalibration
    private let directAligned: UnsafeMutablePointer<Float>
    private let environmentalBasisMapped: UnsafeMutablePointer<Float>
    private let environmentalAligned: UnsafeMutablePointer<Float>
    private let delayBank: AppleSpatialDelayBank
    private let directSampleCount: Int
    private let environmentalSampleCount: Int

    private var mixer: AudioUnit?
    private var cachedSampleTime: Float64 = -.greatestFiniteMagnitude
    private var cachedStatus: OSStatus = noErr
    private var lastDiscontinuitySequence: UInt64 = 0
    private var lastProgramDiscontinuitySequence: UInt64
    private var directWasActive = [Bool](
        repeating: false,
        count: AppleSpatialMixerRenderer.directPlaneCount
    )

    init(
        session: FightboxNeutralSpatialSession,
        provider: FightboxSpatialProgramProvider,
        calibration: AppleSpatialAxisCalibration
    ) {
        self.session = session
        self.provider = provider
        transactionalProvider = provider as? FightboxTransactionalSpatialProgramProvider
        let canonicalProvider = provider as? FightboxCanonicalProgramProvider
        precondition(
            !session.macroProductionBridgeEnabled ||
                (canonicalProvider != nil && !(canonicalProvider?.macroAssetBindings.isEmpty ?? true)),
            "The macro production bridge requires canonical bindings for reserved sources 12...15"
        )
        macroProvider = session.macroProductionBridgeEnabled ? canonicalProvider : nil
        macroCallbackStorage = session.macroProductionBridgeEnabled
            ? FightboxMacroCallbackStorage()
            : nil
        self.calibration = calibration
        lastProgramDiscontinuitySequence =
            transactionalProvider?.programDiscontinuitySequence ?? 0
        programBank = session.makeProgramBank()
        renderStorage = session.makeRenderStorage()
        directSampleCount = Self.directPlaneCount * session.blockSizeFrames
        environmentalSampleCount = Self.environmentalPlaneCount * session.blockSizeFrames
        directAligned = .allocate(capacity: directSampleCount)
        directAligned.initialize(repeating: 0, count: directSampleCount)
        environmentalAligned = .allocate(capacity: environmentalSampleCount)
        environmentalAligned.initialize(repeating: 0, count: environmentalSampleCount)
        environmentalBasisMapped = .allocate(capacity: environmentalSampleCount)
        environmentalBasisMapped.initialize(repeating: 0, count: environmentalSampleCount)
        delayBank = AppleSpatialDelayBank(
            planeCount: Self.directPlaneCount + Self.environmentalPlaneCount,
            maximumDelayFrames: Self.maximumAlignmentFrames
        )
    }

    deinit {
        environmentalBasisMapped.deinitialize(count: environmentalSampleCount)
        environmentalBasisMapped.deallocate()
        environmentalAligned.deinitialize(count: environmentalSampleCount)
        environmentalAligned.deallocate()
        directAligned.deinitialize(count: directSampleCount)
        directAligned.deallocate()
    }

    func attach(mixer: AudioUnit) {
        self.mixer = mixer
    }

    func renderInput(
        flags _: UnsafeMutablePointer<AudioUnitRenderActionFlags>,
        timestamp: UnsafePointer<AudioTimeStamp>,
        bus: UInt32,
        frameCount: UInt32,
        ioData: UnsafeMutablePointer<AudioBufferList>
    ) -> OSStatus {
        guard frameCount == UInt32(session.blockSizeFrames),
              bus <= Self.environmentalBus
        else {
            return kAudio_ParamError
        }

        let sampleTime = timestamp.pointee.mSampleTime
        if sampleTime != cachedSampleTime {
            cachedSampleTime = sampleTime
            cachedStatus = renderNeutralFrame()
        }
        guard cachedStatus == noErr else { return cachedStatus }

        let buffers = UnsafeMutableAudioBufferListPointer(ioData)
        if bus < Self.environmentalBus {
            guard buffers.count == 1 else { return kAudio_ParamError }
            return provide(
                alignedPlane: directAligned.advanced(by: Int(bus) * session.blockSizeFrames),
                frameCount: frameCount,
                to: &buffers[0]
            )
        }

        let channelCount = Int(renderStorage.metadata.pointee.environmental_channel_count)
        guard channelCount > 0,
              channelCount <= Self.environmentalPlaneCount,
              buffers.count == channelCount
        else {
            return kAudio_ParamError
        }
        for channel in 0 ..< channelCount {
            let status = provide(
                alignedPlane: environmentalAligned.advanced(
                    by: channel * session.blockSizeFrames
                ),
                frameCount: frameCount,
                to: &buffers[channel]
            )
            if status != noErr { return status }
        }
        return noErr
    }

    private func provide(
        alignedPlane: UnsafeMutablePointer<Float>,
        frameCount: UInt32,
        to buffer: inout AudioBuffer
    ) -> OSStatus {
        let byteCount = Int(frameCount) * MemoryLayout<Float>.size
        if let destination = buffer.mData {
            memcpy(destination, alignedPlane, byteCount)
        } else {
            buffer.mData = UnsafeMutableRawPointer(alignedPlane)
        }
        buffer.mDataByteSize = UInt32(byteCount)
        return noErr
    }

    private func renderNeutralFrame() -> OSStatus {
        if let transactionalProvider,
           transactionalProvider.programDiscontinuitySequence !=
               lastProgramDiscontinuitySequence
        {
            lastProgramDiscontinuitySequence =
                transactionalProvider.programDiscontinuitySequence
            transactionalProvider.discardFilledBlock()
            delayBank.reset()
            directAligned.update(repeating: 0, count: directSampleCount)
            environmentalAligned.update(repeating: 0, count: environmentalSampleCount)
            return noErr
        }

        var macroTransactionOpen = false
        var macroTransactionEnded = false
        let fillStatus: OSStatus
        if let macroProvider, let macroCallbackStorage {
            let beginStatus = session.beginMacroRender(into: macroCallbackStorage)
            guard beginStatus == noErr else {
                transactionalProvider?.discardFilledBlock()
                return beginStatus
            }
            macroTransactionOpen = true
            fillStatus = macroCallbackStorage.withRequests { requests in
                macroProvider.fillMacroIntervals(requests, into: programBank)
            }
        } else {
            fillStatus = provider.fill(programBank)
        }
        defer {
            if macroTransactionOpen, !macroTransactionEnded {
                _ = session.endMacroRender(commit: false)
            }
        }
        guard fillStatus == noErr else {
            transactionalProvider?.discardFilledBlock()
            return fillStatus
        }

        do {
            try session.render(programs: programBank, into: renderStorage)
        } catch {
            transactionalProvider?.discardFilledBlock()
            return kAudio_ParamError
        }

        let metadata = renderStorage.metadata.pointee
        guard metadata.abi_version == UInt32(FB_ABI_VERSION_V2),
              metadata.sample_rate_hz == session.sampleRateHz,
              metadata.block_size_frames == UInt32(session.blockSizeFrames),
              metadata.environmental_order == session.environmentalOrder,
              metadata.environmental_channel_count == (session.environmentalOrder + 1) *
                  (session.environmentalOrder + 1),
              metadata.environmental_channel_order == FbEnvironmentalAcnV2.rawValue,
              metadata.environmental_normalization == FbEnvironmentalN3dV2.rawValue,
              metadata.environmental_basis == calibration.expectedEnvironmentalBasis.rawValue
        else {
            transactionalProvider?.discardFilledBlock()
            return kAudio_ParamError
        }

        let requiredFlags = UInt32(FB_SPATIAL_BLOCK_VALID_V2) |
            UInt32(FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2) |
            UInt32(FB_SPATIAL_WORLD_UNROTATED_V2) |
            UInt32(FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2) |
            UInt32(FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2) |
            UInt32(FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2)
        guard metadata.flags & requiredFlags == requiredFlags else {
            transactionalProvider?.discardFilledBlock()
            return kAudio_ParamError
        }

        if metadata.validity == FbSpatialSilentDiscontinuityV2.rawValue ||
            metadata.discontinuity_sequence != lastDiscontinuitySequence
        {
            transactionalProvider?.discardFilledBlock()
            lastDiscontinuitySequence = metadata.discontinuity_sequence
            delayBank.reset()
            directAligned.update(repeating: 0, count: directSampleCount)
            environmentalAligned.update(repeating: 0, count: environmentalSampleCount)
            return noErr
        }
        guard metadata.validity == FbSpatialValidV2.rawValue else {
            transactionalProvider?.discardFilledBlock()
            return kAudio_ParamError
        }

        var targetLatency = Int(metadata.environmental_latency_frames)
        for plane in 0 ..< Self.directPlaneCount where renderStorage.feeds[plane].valid != 0 {
            targetLatency = max(
                targetLatency,
                Int(renderStorage.feeds[plane].processing_latency_frames)
            )
        }

        for plane in 0 ..< Self.directPlaneCount {
            let feed = renderStorage.feeds[plane]
            let destination = directAligned.advanced(by: plane * session.blockSizeFrames)
            guard feed.valid != 0 else {
                directWasActive[plane] = false
                destination.update(repeating: 0, count: session.blockSizeFrames)
                continue
            }
            if !directWasActive[plane] {
                delayBank.reset(plane: plane)
                directWasActive[plane] = true
            }
            let delay = targetLatency - Int(feed.processing_latency_frames)
            guard delayBank.process(
                plane: plane,
                input: UnsafePointer(
                    renderStorage.direct.advanced(by: plane * session.blockSizeFrames)
                ),
                output: destination,
                count: session.blockSizeFrames,
                delayFrames: delay
            ) else {
                transactionalProvider?.discardFilledBlock()
                return kAudio_ParamError
            }
            let placementStatus = updatePointPlacement(
                bus: UInt32(plane),
                direction: feed.direction_enu
            )
            guard placementStatus == noErr else {
                transactionalProvider?.discardFilledBlock()
                return placementStatus
            }
        }

        let environmentalChannels = Int(metadata.environmental_channel_count)
        let environmentalDelay = targetLatency - Int(metadata.environmental_latency_frames)
        mapEnvironmentalBasis(channelCount: environmentalChannels)
        for channel in 0 ..< environmentalChannels {
            guard delayBank.process(
                plane: Self.directPlaneCount + channel,
                input: UnsafePointer(
                    environmentalBasisMapped.advanced(
                        by: channel * session.blockSizeFrames
                    )
                ),
                output: environmentalAligned.advanced(by: channel * session.blockSizeFrames),
                count: session.blockSizeFrames,
                delayFrames: environmentalDelay,
                gain: Self.n3dToSn3dGain(acn: channel)
            ) else {
                transactionalProvider?.discardFilledBlock()
                return kAudio_ParamError
            }
        }
        for channel in environmentalChannels ..< Self.environmentalPlaneCount {
            environmentalAligned
                .advanced(by: channel * session.blockSizeFrames)
                .update(repeating: 0, count: session.blockSizeFrames)
        }
        if macroTransactionOpen {
            let endStatus = session.endMacroRender(commit: true)
            macroTransactionEnded = true
            guard endStatus == noErr else {
                transactionalProvider?.discardFilledBlock()
                return endStatus
            }
        }
        transactionalProvider?.commitFilledBlock()
        return noErr
    }

    private func mapEnvironmentalBasis(channelCount: Int) {
        let matrix = calibration.environmentalAcnTransform
        for outputChannel in 0 ..< channelCount {
            let output = environmentalBasisMapped.advanced(
                by: outputChannel * session.blockSizeFrames
            )
            output.update(repeating: 0, count: session.blockSizeFrames)
            for inputChannel in 0 ..< channelCount {
                let coefficient = matrix[outputChannel * 9 + inputChannel]
                if coefficient == 0 { continue }
                let input = renderStorage.environmental.advanced(
                    by: inputChannel * session.blockSizeFrames
                )
                for frame in 0 ..< session.blockSizeFrames {
                    output[frame] += coefficient * input[frame]
                }
            }
        }
    }

    private func updatePointPlacement(bus: UInt32, direction: FbVec3) -> OSStatus {
        guard let mixer else { return kAudioUnitErr_Uninitialized }
        var vector = SIMD3<Float>(direction.east_m, direction.north_m, direction.up_m)
        let length = simd_length(vector)
        if !length.isFinite || length <= 1.0e-6 {
            vector = calibration.frontENU
        } else {
            vector /= length
        }
        let right = simd_dot(vector, calibration.rightENU)
        let front = simd_dot(vector, calibration.frontENU)
        let up = min(max(simd_dot(vector, calibration.upENU), -1), 1)
        let radiansToDegrees = Float(180 / Double.pi)
        let azimuth = atan2f(right, front) * radiansToDegrees
        let elevation = asinf(up) * radiansToDegrees
        let azimuthStatus = AudioUnitSetParameter(
            mixer,
            kSpatialMixerParam_Azimuth,
            kAudioUnitScope_Input,
            bus,
            azimuth,
            0
        )
        guard azimuthStatus == noErr else { return azimuthStatus }
        return AudioUnitSetParameter(
            mixer,
            kSpatialMixerParam_Elevation,
            kAudioUnitScope_Input,
            bus,
            elevation,
            0
        )
    }

    private static func n3dToSn3dGain(acn: Int) -> Float {
        let order = Int(floor(sqrt(Double(acn))))
        return 1 / sqrtf(Float(2 * order + 1))
    }
}

/// iOS 18 production adapter for the neutral 48-object plus one ACN field graph.
///
/// Steam owns propagation and safety processing before this graph. The mixer
/// owns the only final HRTF and automatic AirPods-relative head rotation.
@available(iOS 18.0, *)
public final class AppleSpatialMixerAdapter: @unchecked Sendable {
    private static let sampleRate = 48_000.0
    private static let blockFrames: UInt32 = 128

    private let renderer: AppleSpatialMixerRenderer
    private let calibration: AppleSpatialAxisCalibration
    private let requirements: AppleSpatialRouteRequirements

    private var graph: AUGraph?
    private var mixerUnit: AudioUnit?
    private var running = false
    private var statusValue: AppleSpatialRouteStatus

    public var status: AppleSpatialRouteStatus { statusValue }

    public init(
        session: FightboxNeutralSpatialSession,
        programProvider: FightboxSpatialProgramProvider,
        axisCalibration: AppleSpatialAxisCalibration,
        requirements: AppleSpatialRouteRequirements = AppleSpatialRouteRequirements()
    ) {
        renderer = AppleSpatialMixerRenderer(
            session: session,
            provider: programProvider,
            calibration: axisCalibration
        )
        calibration = axisCalibration
        self.requirements = requirements
        statusValue = AppleSpatialRouteStatus(
            isRunning: false,
            mixerLatencyFrames: 0,
            automaticHeadTracking: .unavailable("mixer not initialized"),
            personalizedHRTF: .unavailable("mixer not initialized"),
            axisCalibrationIdentifier: axisCalibration.evidenceIdentifier,
            axisCalibrationVerified: axisCalibration.isTargetDeviceVerified
        )
    }

    deinit {
        stop()
    }

    public func start(monitorGainDB: Float = 0) throws {
        guard !running else { return }
        guard renderer.session.sampleRateHz == UInt32(Self.sampleRate),
              renderer.session.blockSizeFrames == Int(Self.blockFrames)
        else {
            throw AppleSpatialMixerError.audioUnit(
                operation: "validate 48 kHz / 128-frame neutral contract",
                status: kAudio_ParamError
            )
        }
        if requirements.requireTargetDeviceAxisCalibration,
           !calibration.isTargetDeviceVerified
        {
            throw AppleSpatialMixerError.unverifiedAxisCalibration
        }

        let audioSession = AVAudioSession.sharedInstance()
        try audioSession.setCategory(.playback, mode: .default)
        try audioSession.setPreferredSampleRate(Self.sampleRate)
        try audioSession.setPreferredIOBufferDuration(
            Double(Self.blockFrames) / Self.sampleRate
        )
        try audioSession.setActive(true)
        guard Self.isHeadphoneRoute(audioSession.currentRoute) else {
            try? audioSession.setActive(false, options: .notifyOthersOnDeactivation)
            let ports = audioSession.currentRoute.outputs.map(\.portType.rawValue).joined(separator: ",")
            throw AppleSpatialMixerError.incompatibleOutputRoute(ports)
        }

        do {
            try buildAndInitializeGraph(monitorGainDB: monitorGainDB)
            if requirements.requirePersonalizedHRTFActive,
               statusValue.personalizedHRTF != .active
            {
                throw AppleSpatialMixerError.personalizedHRTFInactive
            }
            guard let graph else {
                throw AppleSpatialMixerError.audioUnit(
                    operation: "retain AUGraph",
                    status: kAudio_ParamError
                )
            }
            try Self.check(AUGraphStart(graph), operation: "AUGraphStart")
            running = true
            statusValue.isRunning = true
        } catch {
            disposeGraph()
            try? audioSession.setActive(false, options: .notifyOthersOnDeactivation)
            throw error
        }
    }

    public func stop() {
        disposeGraph()
        if running {
            try? AVAudioSession.sharedInstance().setActive(
                false,
                options: .notifyOthersOnDeactivation
            )
        }
        running = false
        statusValue.isRunning = false
    }

    /// Applies phone/body attitude once. Automatic AirPods tracking, when
    /// available, adds the separate head-relative rotation inside Apple.
    public func setBodyOrientation(
        yawDegrees: Float,
        pitchDegrees: Float,
        rollDegrees: Float
    ) throws {
        guard let mixerUnit else {
            throw AppleSpatialMixerError.audioUnit(
                operation: "set body orientation before mixer initialization",
                status: kAudioUnitErr_Uninitialized
            )
        }
        try Self.setParameter(
            mixerUnit,
            id: kSpatialMixerParam_HeadYaw,
            value: yawDegrees,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "set body yaw"
        )
        try Self.setParameter(
            mixerUnit,
            id: kSpatialMixerParam_HeadPitch,
            value: pitchDegrees,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "set body pitch"
        )
        try Self.setParameter(
            mixerUnit,
            id: kSpatialMixerParam_HeadRoll,
            value: rollDegrees,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "set body roll"
        )
    }

    public func setMonitorGainDB(_ gainDB: Float) throws {
        guard let mixerUnit else {
            throw AppleSpatialMixerError.audioUnit(
                operation: "set monitor gain before mixer initialization",
                status: kAudioUnitErr_Uninitialized
            )
        }
        let clamped = min(max(gainDB, -60), 0)
        for bus in 0 ... UInt32(FB_MAX_PRESENTATION_FEEDS_V2) {
            try Self.setParameter(
                mixerUnit,
                id: kSpatialMixerParam_Gain,
                value: clamped,
                scope: kAudioUnitScope_Input,
                element: bus,
                operation: "set monitor gain"
            )
        }
    }

    private func buildAndInitializeGraph(monitorGainDB: Float) throws {
        var newGraph: AUGraph?
        try Self.check(NewAUGraph(&newGraph), operation: "NewAUGraph")
        guard let newGraph else {
            throw AppleSpatialMixerError.audioUnit(
                operation: "NewAUGraph returned no graph",
                status: kAudio_ParamError
            )
        }
        graph = newGraph

        var mixerDescription = AudioComponentDescription(
            componentType: kAudioUnitType_Mixer,
            componentSubType: kAudioUnitSubType_SpatialMixer,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0
        )
        var outputDescription = AudioComponentDescription(
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_RemoteIO,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0
        )
        var mixerNode = AUNode()
        var outputNode = AUNode()
        try Self.check(
            AUGraphAddNode(newGraph, &mixerDescription, &mixerNode),
            operation: "add AUSpatialMixer"
        )
        try Self.check(
            AUGraphAddNode(newGraph, &outputDescription, &outputNode),
            operation: "add RemoteIO"
        )
        try Self.check(AUGraphOpen(newGraph), operation: "AUGraphOpen")

        var mixer: AudioUnit?
        var output: AudioUnit?
        try Self.check(
            AUGraphNodeInfo(newGraph, mixerNode, nil, &mixer),
            operation: "get AUSpatialMixer"
        )
        try Self.check(
            AUGraphNodeInfo(newGraph, outputNode, nil, &output),
            operation: "get RemoteIO"
        )
        guard let mixer, let output else {
            throw AppleSpatialMixerError.audioUnit(
                operation: "resolve graph AudioUnits",
                status: kAudio_ParamError
            )
        }
        mixerUnit = mixer
        renderer.attach(mixer: mixer)

        let inputBusCount = UInt32(FB_MAX_PRESENTATION_FEEDS_V2) + 1
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_ElementCount,
            value: inputBusCount,
            scope: kAudioUnitScope_Input,
            element: 0,
            operation: "set 49 mixer input buses"
        )
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_MaximumFramesPerSlice,
            value: Self.blockFrames,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "set mixer maximum slice"
        )
        try Self.setProperty(
            output,
            id: kAudioUnitProperty_MaximumFramesPerSlice,
            value: Self.blockFrames,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "set RemoteIO maximum slice"
        )

        let mono = Self.floatFormat(channels: 1)
        let stereo = Self.floatFormat(channels: 2)
        let environmentChannels = (renderer.session.environmentalOrder + 1) *
            (renderer.session.environmentalOrder + 1)
        let environment = Self.floatFormat(channels: environmentChannels)
        for bus in 0 ..< UInt32(FB_MAX_PRESENTATION_FEEDS_V2) {
            try Self.setProperty(
                mixer,
                id: kAudioUnitProperty_StreamFormat,
                value: mono,
                scope: kAudioUnitScope_Input,
                element: bus,
                operation: "set point input format"
            )
            try Self.setTaggedLayout(
                mixer,
                tag: kAudioChannelLayoutTag_Mono,
                scope: kAudioUnitScope_Input,
                element: bus,
                operation: "set point input layout"
            )
            try configureInputPolicy(
                mixer,
                bus: bus,
                sourceMode: AUSpatialMixerSourceMode
                    .spatialMixerSourceMode_PointSource.rawValue
            )
        }

        let environmentBus = UInt32(FB_MAX_PRESENTATION_FEEDS_V2)
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_StreamFormat,
            value: environment,
            scope: kAudioUnitScope_Input,
            element: environmentBus,
            operation: "set Ambisonic input format"
        )
        let sn3dTag = AudioChannelLayoutTag(kAudioChannelLayoutTag_HOA_ACN_SN3D) |
            AudioChannelLayoutTag(environmentChannels)
        try Self.setTaggedLayout(
            mixer,
            tag: sn3dTag,
            scope: kAudioUnitScope_Input,
            element: environmentBus,
            operation: "set ACN/SN3D Ambisonic layout"
        )
        try configureInputPolicy(
            mixer,
            bus: environmentBus,
            sourceMode: AUSpatialMixerSourceMode
                .spatialMixerSourceMode_AmbienceBed.rawValue
        )
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_StreamFormat,
            value: stereo,
            scope: kAudioUnitScope_Output,
            element: 0,
            operation: "set mixer stereo output format"
        )
        try Self.setTaggedLayout(
            mixer,
            tag: kAudioChannelLayoutTag_Stereo,
            scope: kAudioUnitScope_Output,
            element: 0,
            operation: "set mixer stereo output layout"
        )

        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_SpatialMixerOutputType,
            value: AUSpatialMixerOutputType
                .spatialMixerOutputType_Headphones.rawValue,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "select headphone HRTF output"
        )
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_UsesInternalReverb,
            value: UInt32(0),
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "disable Apple internal reverb"
        )
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_SpatialMixerEnableHeadTracking,
            value: UInt32(1),
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "request automatic AirPods head tracking"
        )
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_SpatialMixerPersonalizedHRTFMode,
            value: AUSpatialMixerPersonalizedHRTFMode.auto.rawValue,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "request personalized HRTF auto mode"
        )

        let callback = AURenderCallbackStruct(
            inputProc: appleSpatialInputCallback,
            inputProcRefCon: Unmanaged.passUnretained(renderer).toOpaque()
        )
        for bus in 0 ..< inputBusCount {
            try Self.setProperty(
                mixer,
                id: kAudioUnitProperty_SetRenderCallback,
                value: callback,
                scope: kAudioUnitScope_Input,
                element: bus,
                operation: "install neutral input callback"
            )
        }

        try Self.check(
            AUGraphConnectNodeInput(newGraph, mixerNode, 0, outputNode, 0),
            operation: "connect spatial mixer to RemoteIO"
        )
        try Self.check(AUGraphInitialize(newGraph), operation: "AUGraphInitialize")

        var latencySeconds = Float64(0)
        try Self.getProperty(
            mixer,
            id: kAudioUnitProperty_Latency,
            value: &latencySeconds,
            scope: kAudioUnitScope_Global,
            element: 0,
            operation: "query spatial mixer latency"
        )
        var personalizedActive = UInt32(0)
        var personalizedActiveSize = UInt32(MemoryLayout<UInt32>.size)
        let personalizedStatus = AudioUnitGetProperty(
            mixer,
            kAudioUnitProperty_SpatialMixerAnyInputIsUsingPersonalizedHRTF,
            kAudioUnitScope_Global,
            0,
            &personalizedActive,
            &personalizedActiveSize
        )
        let personalizedState: AppleSpatialFeatureState
        if personalizedStatus != noErr {
            personalizedState = .unavailable("property query OSStatus \(personalizedStatus)")
        } else if personalizedActive != 0 {
            personalizedState = .active
        } else {
            personalizedState = .unavailable("profile or entitled route is not active")
        }
        statusValue = AppleSpatialRouteStatus(
            isRunning: false,
            mixerLatencyFrames: Int((latencySeconds * Self.sampleRate).rounded()),
            automaticHeadTracking: .requestedUnverified,
            personalizedHRTF: personalizedState,
            axisCalibrationIdentifier: calibration.evidenceIdentifier,
            axisCalibrationVerified: calibration.isTargetDeviceVerified
        )

        let clampedMonitorGainDB = min(max(monitorGainDB, -60), 0)
        for bus in 0 ..< inputBusCount {
            try Self.setParameter(
                mixer,
                id: kSpatialMixerParam_Gain,
                value: clampedMonitorGainDB,
                scope: kAudioUnitScope_Input,
                element: bus,
                operation: "set monitor gain"
            )
        }
    }

    private func configureInputPolicy(
        _ mixer: AudioUnit,
        bus: UInt32,
        sourceMode: UInt32
    ) throws {
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_SpatialMixerSourceMode,
            value: sourceMode,
            scope: kAudioUnitScope_Input,
            element: bus,
            operation: "set spatial source mode"
        )
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_SpatializationAlgorithm,
            value: AUSpatializationAlgorithm
                .spatializationAlgorithm_UseOutputType.rawValue,
            scope: kAudioUnitScope_Input,
            element: bus,
            operation: "select output-type spatialization"
        )
        try Self.setProperty(
            mixer,
            id: kAudioUnitProperty_SpatialMixerRenderingFlags,
            value: AUSpatialMixerRenderingFlags
                .spatialMixerRenderingFlags_InterAuralDelay.rawValue,
            scope: kAudioUnitScope_Input,
            element: bus,
            operation: "disable duplicate distance processing"
        )
        try Self.setParameter(
            mixer,
            id: kSpatialMixerParam_Distance,
            value: 1,
            scope: kAudioUnitScope_Input,
            element: bus,
            operation: "set neutral one-meter mixer distance"
        )
        try Self.setParameter(
            mixer,
            id: kSpatialMixerParam_ReverbBlend,
            value: 0,
            scope: kAudioUnitScope_Input,
            element: bus,
            operation: "disable duplicate reverb"
        )
        try Self.setParameter(
            mixer,
            id: kSpatialMixerParam_OcclusionAttenuation,
            value: 0,
            scope: kAudioUnitScope_Input,
            element: bus,
            operation: "disable duplicate occlusion"
        )
        try Self.setParameter(
            mixer,
            id: kSpatialMixerParam_ObstructionAttenuation,
            value: 0,
            scope: kAudioUnitScope_Input,
            element: bus,
            operation: "disable duplicate obstruction"
        )
    }

    private func disposeGraph() {
        if let graph {
            _ = AUGraphStop(graph)
            _ = AUGraphUninitialize(graph)
            _ = AUGraphClose(graph)
            DisposeAUGraph(graph)
        }
        graph = nil
        mixerUnit = nil
    }

    private static func isHeadphoneRoute(_ route: AVAudioSessionRouteDescription) -> Bool {
        route.outputs.contains { output in
            switch output.portType {
            case .headphones, .bluetoothA2DP, .bluetoothLE:
                return true
            default:
                return false
            }
        }
    }

    private static func floatFormat(channels: UInt32) -> AudioStreamBasicDescription {
        var format = AudioStreamBasicDescription()
        format.mSampleRate = sampleRate
        format.mFormatID = kAudioFormatLinearPCM
        format.mFormatFlags = kAudioFormatFlagsNativeFloatPacked |
            kAudioFormatFlagIsNonInterleaved
        format.mBytesPerPacket = UInt32(MemoryLayout<Float>.size)
        format.mFramesPerPacket = 1
        format.mBytesPerFrame = UInt32(MemoryLayout<Float>.size)
        format.mChannelsPerFrame = channels
        format.mBitsPerChannel = UInt32(MemoryLayout<Float>.size * 8)
        return format
    }

    private static func setTaggedLayout(
        _ unit: AudioUnit,
        tag: AudioChannelLayoutTag,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        operation: String
    ) throws {
        var layout = AudioChannelLayout()
        layout.mChannelLayoutTag = tag
        try setProperty(
            unit,
            id: kAudioUnitProperty_AudioChannelLayout,
            value: layout,
            scope: scope,
            element: element,
            operation: operation
        )
    }

    private static func setProperty<T>(
        _ unit: AudioUnit,
        id: AudioUnitPropertyID,
        value: T,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        operation: String
    ) throws {
        var value = value
        let status = withUnsafeBytes(of: &value) { bytes in
            AudioUnitSetProperty(
                unit,
                id,
                scope,
                element,
                bytes.baseAddress!,
                UInt32(bytes.count)
            )
        }
        try check(status, operation: operation)
    }

    private static func getProperty<T>(
        _ unit: AudioUnit,
        id: AudioUnitPropertyID,
        value: inout T,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        operation: String
    ) throws {
        var size = UInt32(MemoryLayout<T>.size)
        let status = withUnsafeMutableBytes(of: &value) { bytes in
            AudioUnitGetProperty(
                unit,
                id,
                scope,
                element,
                bytes.baseAddress!,
                &size
            )
        }
        try check(status, operation: operation)
    }

    private static func setParameter(
        _ unit: AudioUnit,
        id: AudioUnitParameterID,
        value: AudioUnitParameterValue,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        operation: String
    ) throws {
        try check(
            AudioUnitSetParameter(unit, id, scope, element, value, 0),
            operation: operation
        )
    }

    private static func check(_ status: OSStatus, operation: String) throws {
        guard status == noErr else {
            throw AppleSpatialMixerError.audioUnit(operation: operation, status: status)
        }
    }
}
#endif
