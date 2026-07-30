import FightboxC
import Foundation

public enum FightboxError: Error, Sendable, CustomStringConvertible {
    case ffi(code: Int32)
    case invalidBufferCount(expected: Int, actual: Int)
    case invalidTelemetry

    public var description: String {
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

/// Swift ownership wrapper for one retained Fightbox session.
///
/// Call `updateListener`, `updateSource`, and `telemetryJSON` from one
/// serialized control queue. Call `render` from one audio callback thread.
/// Stop and join both roles before releasing the final reference.
public final class FightboxSession: @unchecked Sendable {
    public let sampleRateHz: UInt32
    public let blockSizeFrames: Int
    public let sourceCount: Int

    private let handle: OpaquePointer

    public init(
        sampleRateHz: UInt32 = 48_000,
        blockSizeFrames: UInt32 = 128,
        sourceCount: UInt32,
        defaultSourceLevelDB: Float = 0,
        packageURL: URL,
        bakeURL: URL
    ) throws {
        var config = FbSessionConfig()
        config.sample_rate_hz = sampleRateHz
        config.block_size_frames = blockSizeFrames
        config.source_count = sourceCount
        config.default_source_level_db = defaultSourceLevelDB

        var newHandle: OpaquePointer?
        let result = packageURL.path.withCString { packagePath in
            bakeURL.path.withCString { bakePath in
                fb_session_create(&config, packagePath, bakePath, &newHandle)
            }
        }
        try Self.check(result)
        guard let newHandle else {
            throw FightboxError.ffi(code: Int32(FbInvalidState.rawValue))
        }
        self.handle = newHandle
        self.sampleRateHz = sampleRateHz
        self.blockSizeFrames = Int(blockSizeFrames)
        self.sourceCount = Int(sourceCount)
    }

    deinit {
        let result = fb_session_destroy(handle)
        assert(result.rawValue == FbOk.rawValue, "Fightbox session destroy failed")
    }

    public func updateListener(
        pose: FightboxPose,
        linearVelocityMPS: SIMD3<Float> = .zero
    ) throws {
        var ffiPose = pose.ffi
        var velocity = linearVelocityMPS.ffi
        try Self.check(fb_session_update_listener(handle, &ffiPose, &velocity))
    }

    public func updateSource(
        index: UInt32,
        active: Bool,
        pose: FightboxPose,
        linearVelocityMPS: SIMD3<Float> = .zero
    ) throws {
        var update = FbSourceUpdate()
        update.active = active ? 1 : 0
        update.pose = pose.ffi
        update.linear_velocity_mps = linearVelocityMPS.ffi
        try Self.check(fb_session_update_source(handle, index, &update))
    }

    /// Renders source-major mono input into interleaved stereo output.
    ///
    /// This method performs no Swift allocation when both arrays already have
    /// their exact required sizes.
    public func render(
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

    public func telemetryJSON() throws -> String {
        var required = 0
        let sizing = fb_session_telemetry_json(handle, nil, 0, &required)
        guard sizing.rawValue == FbBufferTooSmall.rawValue, required > 0 else {
            try Self.check(sizing)
            throw FightboxError.invalidTelemetry
        }
        var buffer = [CChar](repeating: 0, count: required)
        let result = buffer.withUnsafeMutableBufferPointer { bytes in
            fb_session_telemetry_json(handle, bytes.baseAddress, bytes.count, &required)
        }
        try Self.check(result)
        let payload = buffer.dropLast().map { UInt8(bitPattern: $0) }
        guard let text = String(bytes: payload, encoding: .utf8) else {
            throw FightboxError.invalidTelemetry
        }
        return text
    }

    private static func check(_ result: FbResult) throws {
        guard result.rawValue == FbOk.rawValue else {
            throw FightboxError.ffi(code: Int32(result.rawValue))
        }
    }
}

public struct FightboxPose: Sendable {
    public var position: SIMD3<Float>
    public var forward: SIMD3<Float>
    public var up: SIMD3<Float>

    public init(
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
