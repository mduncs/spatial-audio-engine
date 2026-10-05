import Foundation

#if os(iOS)
import CoreMotion
import simd

/// Minimal control-queue sample that feeds device orientation into Fightbox.
///
/// The sample keeps listener position fixed. The reference attitude captured
/// by `start` becomes ENU identity: screen-top is north and screen-normal is up.
@available(iOS 15.0, *)
public final class CoreMotionHeadTracker: @unchecked Sendable {
    private let motionManager = CMMotionManager()
    private let motionQueue: OperationQueue
    private let session: FightboxSession
    private var referenceAttitude: simd_quatf?

    public var listenerPositionENU = SIMD3<Float>.zero
    public var onError: (@Sendable (Error) -> Void)?

    public init(session: FightboxSession) {
        self.session = session
        motionQueue = OperationQueue()
        motionQueue.name = "fightbox.head-tracking.control"
        motionQueue.maxConcurrentOperationCount = 1
        motionQueue.qualityOfService = .userInteractive
    }

    public func start(updateRateHz: Double = 60) throws {
        guard motionManager.isDeviceMotionAvailable else {
            throw CoreMotionHeadTrackerError.deviceMotionUnavailable
        }
        guard updateRateHz.isFinite, updateRateHz > 0 else {
            throw CoreMotionHeadTrackerError.invalidUpdateRate
        }
        referenceAttitude = nil
        motionManager.deviceMotionUpdateInterval = 1 / updateRateHz
        motionManager.startDeviceMotionUpdates(
            using: .xArbitraryCorrectedZVertical,
            to: motionQueue
        ) { [weak self] motion, error in
            guard let self else { return }
            if let error {
                self.onError?(error)
                return
            }
            guard let motion else { return }
            do {
                try self.consume(motion)
            } catch {
                self.onError?(error)
            }
        }
    }

    public func stop() {
        motionManager.stopDeviceMotionUpdates()
        referenceAttitude = nil
    }

    private func consume(_ motion: CMDeviceMotion) throws {
        let attitude = simd_quatf(
            ix: Float(motion.attitude.quaternion.x),
            iy: Float(motion.attitude.quaternion.y),
            iz: Float(motion.attitude.quaternion.z),
            r: Float(motion.attitude.quaternion.w)
        )
        if referenceAttitude == nil {
            referenceAttitude = attitude
        }
        guard let referenceAttitude else { return }
        let relative = referenceAttitude.inverse * attitude
        let forward = relative.act(SIMD3<Float>(0, 1, 0))
        let up = relative.act(SIMD3<Float>(0, 0, 1))
        try session.updateListener(
            pose: FightboxPose(
                position: listenerPositionENU,
                forward: forward,
                up: up
            )
        )
    }
}

public enum CoreMotionHeadTrackerError: Error, Sendable {
    case deviceMotionUnavailable
    case invalidUpdateRate
}
#endif
