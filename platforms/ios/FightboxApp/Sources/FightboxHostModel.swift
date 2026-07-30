import Combine
import Foundation

@MainActor
final class FightboxHostModel: ObservableObject {
    @Published private(set) var isRunning = false
    @Published private(set) var lifecycleStatus = "Not started"
    @Published private(set) var motionStatus = "Not started"
    @Published private(set) var gpsStatus = "Not started"
    @Published private(set) var telemetryText = "Telemetry unavailable"
    @Published private(set) var monitorGainDB: Double = -12

    private var session: FightboxSession?
    private var audioHost: FightboxAudioHost?
    private var headTracker: CoreMotionHeadTracker?
    private var gpsProvider: GpsLocalEnuProvider?
    private var telemetryTimer: Timer?

    func start() {
        guard !isRunning else { return }
        lifecycleStatus = "Starting Mobile-tier session…"

        do {
            if session == nil {
                try configure()
            }
            guard let audioHost, let headTracker, let gpsProvider else {
                throw FightboxHostError.incompleteConfiguration
            }

            try audioHost.start(monitorGainDB: Float(monitorGainDB))
            gpsProvider.start()
            do {
                try headTracker.start(updateRateHz: 60)
                motionStatus = "Tracking at 60 Hz"
            } catch {
                motionStatus = "Unavailable: \(error)"
            }

            isRunning = true
            lifecycleStatus = "Running · 48 kHz · 128 frames · Mobile"
            beginTelemetryPolling()
            refreshTelemetry()
        } catch {
            audioHost?.stop()
            gpsProvider?.stop()
            headTracker?.stop()
            lifecycleStatus = "Start failed: \(error)"
            isRunning = false
        }
    }

    func stop() {
        telemetryTimer?.invalidate()
        telemetryTimer = nil
        audioHost?.stop()
        gpsProvider?.stop()
        headTracker?.stop()
        isRunning = false
        lifecycleStatus = "Stopped"
        motionStatus = "Stopped"
        gpsStatus = "Stopped"
    }

    func setMonitorGainDB(_ value: Double) {
        monitorGainDB = min(max(value, -60), 0)
        audioHost?.setMonitorGainDB(Float(monitorGainDB))
    }

    private func configure() throws {
        guard let packageURL = Bundle.main.url(
            forResource: "chicago-block-a",
            withExtension: "fightbox"
        ) else {
            throw FightboxHostError.missingBundledResource(
                "chicago-block-a.fightbox"
            )
        }
        guard let bakeURL = Bundle.main.url(
            forResource: "chicago-block-baked",
            withExtension: nil
        ) else {
            throw FightboxHostError.missingBundledResource(
                "chicago-block-baked"
            )
        }

        let session = try FightboxSession(
            sampleRateHz: 48_000,
            blockSizeFrames: 128,
            sourceCount: 1,
            defaultSourceLevelDB: 0,
            qualityTier: FbQualityMobile,
            packageURL: packageURL,
            bakeURL: bakeURL
        )
        try session.updateSource(
            index: 0,
            active: true,
            pose: FightboxPose(
                position: SIMD3<Float>(0, 2, 0),
                forward: SIMD3<Float>(0, -1, 0),
                up: SIMD3<Float>(0, 0, 1)
            )
        )

        let headTracker = CoreMotionHeadTracker(session: session)
        headTracker.onError = { [weak self] error in
            Task { @MainActor in
                self?.motionStatus = "Motion error: \(error)"
            }
        }

        let gpsProvider = GpsLocalEnuProvider()
        gpsProvider.onStateChange = { [weak self, weak headTracker] state in
            if case let .valid(reading) = state {
                headTracker?.listenerPositionENU = reading.positionENU
            }
            Task { @MainActor in
                self?.gpsStatus = Self.describeGpsState(state)
            }
        }

        self.session = session
        self.headTracker = headTracker
        self.gpsProvider = gpsProvider
        audioHost = try FightboxAudioHost(session: session)
    }

    private func beginTelemetryPolling() {
        telemetryTimer?.invalidate()
        telemetryTimer = Timer.scheduledTimer(
            withTimeInterval: 1,
            repeats: true
        ) { [weak self] _ in
            Task { @MainActor in
                self?.refreshTelemetry()
            }
        }
    }

    private func refreshTelemetry() {
        guard let session else { return }
        do {
            telemetryText = Self.prettyJSON(try session.telemetryJSON())
        } catch {
            telemetryText = "Telemetry error: \(error)"
        }
    }

    private static func prettyJSON(_ rawJSON: String) -> String {
        guard let data = rawJSON.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              let pretty = try? JSONSerialization.data(
                  withJSONObject: object,
                  options: [.prettyPrinted, .sortedKeys]
              ),
              let text = String(data: pretty, encoding: .utf8)
        else {
            return rawJSON
        }
        return text
    }

    private static func describeGpsState(_ state: GpsLocalEnuState) -> String {
        switch state {
        case .waitingForAuthorization:
            return "Waiting for location permission"
        case .waitingForAcceptedFix:
            return "Waiting for a ≤20 m fix"
        case let .valid(reading):
            return String(
                format: "Valid · %.1f m accuracy · ENU %.1f, %.1f, %.1f m",
                reading.horizontalAccuracyM,
                reading.positionENU.x,
                reading.positionENU.y,
                reading.positionENU.z
            )
        case let .stale(reading):
            return String(
                format: "Stale · last accuracy %.1f m",
                reading.horizontalAccuracyM
            )
        case let .invalid(reason):
            switch reason {
            case .locationServicesDisabled:
                return "Location services disabled"
            case .authorizationDenied:
                return "Location permission denied"
            case .invalidCoordinate:
                return "Invalid location coordinate"
            case let .horizontalAccuracyM(accuracy):
                return String(format: "Fix rejected · %.1f m accuracy", accuracy)
            case let .locationManagerError(message):
                return "Location error: \(message)"
            }
        }
    }
}

enum FightboxHostError: Error, CustomStringConvertible {
    case missingBundledResource(String)
    case incompleteConfiguration

    var description: String {
        switch self {
        case let .missingBundledResource(name):
            return "Missing bundled resource \(name)"
        case .incompleteConfiguration:
            return "Fightbox host configuration is incomplete"
        }
    }
}
