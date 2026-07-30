import Foundation

enum FightboxError: Error, CustomStringConvertible {
    case ffi(code: Int32)
    case invalidBufferCount(expected: Int, actual: Int)
    case invalidTelemetry

    var description: String {
        switch self {
        case let .ffi(code):
            return "Fightbox C API returned status \(code)"
        case let .invalidBufferCount(expected, actual):
            return "Expected \(expected) samples, received \(actual)"
        case .invalidTelemetry:
            return "Fightbox returned invalid UTF-8 telemetry"
        }
    }
}

/// App-owned session wrapper around the frozen C ABI.
///
/// All control-side C calls hop synchronously onto `controlQueue`. Rendering
/// bypasses that queue and is called only by the AVAudioSourceNode callback.
public final class FightboxSession: @unchecked Sendable {
    let sampleRateHz: UInt32
    let blockSizeFrames: Int
    let sourceCount: Int
    let qualityTier: FbQualityTier

    private let handle: OpaquePointer
    private let controlQueue: DispatchQueue

    init(
        sampleRateHz: UInt32 = 48_000,
        blockSizeFrames: UInt32 = 128,
        sourceCount: UInt32,
        defaultSourceLevelDB: Float = 0,
        qualityTier: FbQualityTier,
        packageURL: URL,
        bakeURL: URL
    ) throws {
        let queue = DispatchQueue(
            label: "fightbox.session.control",
            qos: .userInteractive
        )
        var config = FbSessionConfig()
        config.sample_rate_hz = sampleRateHz
        config.block_size_frames = blockSizeFrames
        config.source_count = sourceCount
        config.default_source_level_db = defaultSourceLevelDB
        config.quality_tier = UInt32(qualityTier.rawValue)

        var newHandle: OpaquePointer?
        let result = queue.sync {
            packageURL.path.withCString { packagePath in
                bakeURL.path.withCString { bakePath in
                    fb_session_create(&config, packagePath, bakePath, &newHandle)
                }
            }
        }
        try Self.check(result)
        guard let newHandle else {
            throw FightboxError.ffi(code: Int32(FbInvalidState.rawValue))
        }

        controlQueue = queue
        handle = newHandle
        self.sampleRateHz = sampleRateHz
        self.blockSizeFrames = Int(blockSizeFrames)
        self.sourceCount = Int(sourceCount)
        self.qualityTier = qualityTier
    }

    deinit {
        let result = controlQueue.sync {
            fb_session_destroy(handle)
        }
        assert(result.rawValue == FbOk.rawValue, "Fightbox session destroy failed")
    }

    func updateListener(
        pose: FightboxPose,
        linearVelocityMPS: SIMD3<Float> = .zero
    ) throws {
        try controlQueue.sync {
            var ffiPose = pose.ffi
            var velocity = linearVelocityMPS.ffi
            try Self.check(fb_session_update_listener(handle, &ffiPose, &velocity))
        }
    }

    func updateSource(
        index: UInt32,
        active: Bool,
        pose: FightboxPose,
        linearVelocityMPS: SIMD3<Float> = .zero
    ) throws {
        try controlQueue.sync {
            var update = FbSourceUpdate()
            update.active = active ? 1 : 0
            update.pose = pose.ffi
            update.linear_velocity_mps = linearVelocityMPS.ffi
            try Self.check(fb_session_update_source(handle, index, &update))
        }
    }

    /// Render exactly one configured block on the audio thread.
    func render(
        sourceMajorMono: [Float],
        into interleavedStereo: inout [Float]
    ) throws {
        let expectedInput = sourceCount * blockSizeFrames
        guard sourceMajorMono.count == expectedInput else {
            throw FightboxError.invalidBufferCount(
                expected: expectedInput,
                actual: sourceMajorMono.count
            )
        }
        let expectedOutput = blockSizeFrames * 2
        guard interleavedStereo.count == expectedOutput else {
            throw FightboxError.invalidBufferCount(
                expected: expectedOutput,
                actual: interleavedStereo.count
            )
        }

        let result = sourceMajorMono.withUnsafeBufferPointer { input in
            interleavedStereo.withUnsafeMutableBufferPointer { output in
                fb_session_render_block(
                    handle,
                    input.baseAddress,
                    input.count,
                    output.baseAddress,
                    output.count
                )
            }
        }
        try Self.check(result)
    }

    func telemetryJSON() throws -> String {
        try controlQueue.sync {
            var required = 0
            let sizing = fb_session_telemetry_json(handle, nil, 0, &required)
            guard sizing.rawValue == FbBufferTooSmall.rawValue, required > 0 else {
                try Self.check(sizing)
                throw FightboxError.invalidTelemetry
            }

            var buffer = [CChar](repeating: 0, count: required)
            let result = buffer.withUnsafeMutableBufferPointer { bytes in
                fb_session_telemetry_json(
                    handle,
                    bytes.baseAddress,
                    bytes.count,
                    &required
                )
            }
            try Self.check(result)
            let payload = buffer.dropLast().map { UInt8(bitPattern: $0) }
            guard let text = String(bytes: payload, encoding: .utf8) else {
                throw FightboxError.invalidTelemetry
            }
            return text
        }
    }

    private static func check(_ result: FbResult) throws {
        guard result.rawValue == FbOk.rawValue else {
            throw FightboxError.ffi(code: Int32(result.rawValue))
        }
    }
}

struct FightboxPose: Sendable {
    var position: SIMD3<Float>
    var forward: SIMD3<Float>
    var up: SIMD3<Float>

    init(
        position: SIMD3<Float>,
        forward: SIMD3<Float>,
        up: SIMD3<Float>
    ) {
        self.position = position
        self.forward = forward
        self.up = up
    }

    fileprivate var ffi: FbPose {
        var value = FbPose()
        value.position = position.ffi
        value.forward = forward.ffi
        value.up = up.ffi
        return value
    }
}

private extension SIMD3 where Scalar == Float {
    var ffi: FbVec3 {
        var value = FbVec3()
        value.east_m = x
        value.north_m = y
        value.up_m = z
        return value
    }
}
