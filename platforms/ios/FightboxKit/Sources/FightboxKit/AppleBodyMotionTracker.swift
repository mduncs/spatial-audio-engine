#if os(iOS)
import CoreMotion
import Foundation
import simd

/// Supplies phone/body attitude to the Apple route exactly once.
///
/// This deliberately uses `CMMotionManager`, not
/// `CMHeadphoneMotionManager`. The latter would duplicate the automatic
/// AirPods-relative rotation owned by `AUSpatialMixer`.
@available(iOS 18.0, *)
public final class AppleBodyMotionTracker: @unchecked Sendable {
    private let motionManager = CMMotionManager()
    private let motionQueue: OperationQueue
    private let adapter: AppleSpatialMixerAdapter
    private let session: FightboxNeutralSpatialSession
    private let stateLock = NSLock()
    private var storedListenerPositionENU = SIMD3<Float>.zero
    private var referenceAttitude: simd_quatf?
    private var motionGeneration: UInt64 = 0
    private var trackingActive = false

    public var listenerPositionENU: SIMD3<Float> {
        get {
            stateLock.lock()
            defer { stateLock.unlock() }
            return storedListenerPositionENU
        }
        set {
            stateLock.lock()
            storedListenerPositionENU = newValue
            stateLock.unlock()
        }
    }
    public var onError: (@Sendable (Error) -> Void)?

    public init(
        adapter: AppleSpatialMixerAdapter,
        session: FightboxNeutralSpatialSession
    ) {
        self.adapter = adapter
        self.session = session
        motionQueue = OperationQueue()
        motionQueue.name = "fightbox.apple-body-motion.control"
        motionQueue.maxConcurrentOperationCount = 1
        motionQueue.qualityOfService = .userInteractive
    }

    public func start(updateRateHz: Double = 60) throws {
        guard motionManager.isDeviceMotionAvailable else {
            throw AppleBodyMotionTrackerError.deviceMotionUnavailable
        }
        guard updateRateHz.isFinite, updateRateHz > 0 else {
            throw AppleBodyMotionTrackerError.invalidUpdateRate
        }

        stateLock.lock()
        motionGeneration &+= 1
        if motionGeneration == 0 { motionGeneration = 1 }
        let generation = motionGeneration
        trackingActive = true
        referenceAttitude = nil
        stateLock.unlock()
        motionManager.deviceMotionUpdateInterval = 1 / updateRateHz
        motionManager.startDeviceMotionUpdates(
            using: .xArbitraryCorrectedZVertical,
            to: motionQueue
        ) { [weak self] motion, error in
            guard let self else { return }
            if let error {
                if self.isTracking(generation: generation) {
                    self.onError?(error)
                }
                return
            }
            guard let motion else { return }
            do {
                try self.consume(motion, generation: generation)
            } catch {
                if self.isTracking(generation: generation) {
                    self.onError?(error)
                }
            }
        }
    }

    public func stop() {
        motionManager.stopDeviceMotionUpdates()
        stateLock.lock()
        trackingActive = false
        motionGeneration &+= 1
        if motionGeneration == 0 { motionGeneration = 1 }
        referenceAttitude = nil
        stateLock.unlock()
    }

    private func isTracking(generation: UInt64) -> Bool {
        stateLock.lock()
        defer { stateLock.unlock() }
        return trackingActive && motionGeneration == generation
    }

    private func consume(
        _ motion: CMDeviceMotion,
        generation: UInt64
    ) throws {
        let attitude = simd_quatf(
            ix: Float(motion.attitude.quaternion.x),
            iy: Float(motion.attitude.quaternion.y),
            iz: Float(motion.attitude.quaternion.z),
            r: Float(motion.attitude.quaternion.w)
        )
        stateLock.lock()
        defer { stateLock.unlock() }
        guard trackingActive, motionGeneration == generation else { return }
        if referenceAttitude == nil {
            referenceAttitude = attitude
        }
        guard let referenceAttitude else { return }
        let listenerPositionENU = storedListenerPositionENU

        let relative = referenceAttitude.inverse * attitude
        let forward = relative.act(SIMD3<Float>(0, 1, 0))
        let up = relative.act(SIMD3<Float>(0, 0, 1))
        let right = relative.act(SIMD3<Float>(1, 0, 0))
        let degrees = Float(180 / Double.pi)
        let yaw = atan2f(forward.x, forward.y) * degrees
        let pitch = asinf(min(max(forward.z, -1), 1)) * degrees
        let roll = atan2f(right.z, up.z) * degrees

        try adapter.setBodyOrientation(
            yawDegrees: yaw,
            pitchDegrees: pitch,
            rollDegrees: roll
        )
        try session.updateListener(
            pose: FightboxPose(
                position: listenerPositionENU,
                forward: forward,
                up: up
            )
        )
    }
}

public enum AppleBodyMotionTrackerError: Error, Sendable {
    case deviceMotionUnavailable
    case invalidUpdateRate
}
#endif
