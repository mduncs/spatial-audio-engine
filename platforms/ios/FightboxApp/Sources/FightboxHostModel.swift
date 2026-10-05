import Combine
import Foundation

@MainActor
final class FightboxHostModel: ObservableObject {
    @Published private(set) var isRunning = false
    @Published private(set) var lifecycleStatus = "Not started"
    @Published private(set) var outputRouteStatus = FightboxHostModel.initialOutputRouteStatus
    @Published private(set) var cellStreamingStatus = FightboxHostModel.initialCellStreamingStatus
    @Published private(set) var canonicalAssetStatus = FightboxHostModel.initialCanonicalAssetStatus
    @Published private(set) var selectedSourceContractID =
        FightboxNamedSourceContract.tomsDinerMonoReference.id
    @Published private(set) var motionStatus = "Not started"
    @Published private(set) var gpsStatus = "Not started"
    @Published private(set) var telemetryText = "Telemetry unavailable"
    @Published private(set) var monitorGainDB: Double = -12
    @Published private(set) var routeQualificationAvailable = false
    @Published private(set) var routeQualificationInProgress = false

    private var session: FightboxSession?
    private var audioHost: FightboxAudioHost?
    private var headTracker: CoreMotionHeadTracker?
    private var gpsProvider: GpsLocalEnuProvider?
    private var telemetryTimer: Timer?
    private var startTask: Task<Void, Never>?
    private var stopTask: Task<Void, Never>?
    private var routeQualificationTask: Task<Void, Never>?
    private var restartBlockedByFailedClosedHost = false
    private var acceptsExternalPositionControl = true
    private var appleSpatialHostStorage: AnyObject?
    private var appleRouteRuntimeStorage: AnyObject?
    @available(iOS 18.0, *)
    private var appleSpatialHost: FightboxAppleSpatialProductionHost? {
        get { appleSpatialHostStorage as? FightboxAppleSpatialProductionHost }
        set { appleSpatialHostStorage = newValue }
    }
    @available(iOS 18.0, *)
    private var appleRouteRuntime: FightboxAppleCityRouteRuntime? {
        get { appleRouteRuntimeStorage as? FightboxAppleCityRouteRuntime }
        set { appleRouteRuntimeStorage = newValue }
    }

    let namedSourceContracts = FightboxNamedSourceContract.tomsDinerChoices

    func start() {
        guard !isRunning,
              startTask == nil,
              stopTask == nil,
              !restartBlockedByFailedClosedHost
        else { return }
        lifecycleStatus = "Starting Mobile-tier session…"
        startTask = Task { [weak self] in
            guard let self else { return }
            await self.startSelectedRoute()
            self.startTask = nil
        }
    }

    func stop() {
        guard stopTask == nil else { return }
        let pendingStart = startTask
        let pendingRouteQualification = routeQualificationTask
        pendingStart?.cancel()
        pendingRouteQualification?.cancel()
        telemetryTimer?.invalidate()
        telemetryTimer = nil
        audioHost?.stop()
        gpsProvider?.stop()
        headTracker?.stop()
        lifecycleStatus = "Stopping · draining committed tails…"

        if #available(iOS 18.0, *) {
            let host = appleSpatialHost
            stopTask = Task { [weak self, pendingStart, pendingRouteQualification, host] in
                await pendingStart?.value
                await pendingRouteQualification?.value
                guard let self else { return }
                let finalHost = self.appleSpatialHost ?? host
                finalHost?.stop()
                let phase = await finalHost?.stopAndWait()
                self.finishStop(applePhase: phase)
            }
        } else {
            stopTask = Task { [weak self, pendingStart, pendingRouteQualification] in
                await pendingStart?.value
                await pendingRouteQualification?.value
                self?.finishStop(applePhase: nil)
            }
        }
    }

    private func finishStop(applePhase: FightboxMacroLifecyclePhase?) {
        audioHost = nil
        gpsProvider = nil
        headTracker = nil
        session = nil
        startTask = nil
        stopTask = nil
        routeQualificationTask = nil
        routeQualificationAvailable = false
        routeQualificationInProgress = false
        acceptsExternalPositionControl = true
        appleRouteRuntimeStorage = nil
        motionStatus = "Stopped"
        gpsStatus = "Stopped"
        if applePhase == .failedClosed {
            restartBlockedByFailedClosedHost = true
            // Keep the old host visible and strongly retained. Its callback and
            // session intentionally remain live; a new host must not overlap it.
            isRunning = true
            lifecycleStatus = "Failed closed · restart blocked until process teardown"
            return
        }
        if #available(iOS 18.0, *) {
            appleSpatialHost = nil
        }
        isRunning = false
        lifecycleStatus = "Stopped"
    }

    func setMonitorGainDB(_ value: Double) {
        monitorGainDB = min(max(value, -60), 0)
        if #available(iOS 18.0, *), let appleSpatialHost {
            try? appleSpatialHost.setMonitorGainDB(Float(monitorGainDB))
        }
        audioHost?.setMonitorGainDB(Float(monitorGainDB))
    }

    func selectNamedSourceContract(_ id: String) {
        guard let selection = namedSourceContracts.first(where: { $0.id == id }) else { return }
        selectedSourceContractID = selection.id
        let suffix = isRunning ? " · applies after restart" : ""
        switch selection.expectedProvenance {
        case .nativeMono:
            canonicalAssetStatus =
                "Tom's Diner mono contract selected · point HRTF output · licensed package not bundled\(suffix)"
        case .authoredStereo:
            canonicalAssetStatus =
                "Tom's Diner authored L/R selected · StereoImage preserves both channels · licensed package not bundled\(suffix)"
        case .monoExpanded:
            canonicalAssetStatus =
                "MonoExpanded selected · frozen absolute-frame derived width\(suffix)"
        }
    }

    /// Receives the already-authoritative Rust macro seek frame. No distance or
    /// speed-of-sound calculation is repeated in the iOS host.
    func applyMacroArrival(programSeekFrame: UInt64) {
        guard #available(iOS 18.0, *), let appleSpatialHost else { return }
        Task { [weak self, weak appleSpatialHost] in
            do {
                try await appleSpatialHost?.applyMacroArrival(
                    programSeekFrame: programSeekFrame
                )
                await MainActor.run {
                    self?.canonicalAssetStatus =
                        "Macro ingress seek published at original frame \(programSeekFrame)"
                }
            } catch {
                await MainActor.run {
                    self?.canonicalAssetStatus = "Macro ingress seek failed: \(error)"
                }
            }
        }
    }

    private func startSelectedRoute() async {
        var appleFailure: String?
        if #available(iOS 18.0, *) {
            do {
                let installedRoute = try await Task.detached(priority: .utility) {
                    try FightboxInstalledCityRoute.loadQualificationRoute()
                }.value
                guard let sourceContract = namedSourceContracts.first(
                    where: { $0.id == selectedSourceContractID }
                ) else {
                    throw FightboxHostError.incompleteConfiguration
                }
                let host = try await FightboxAppleSpatialProductionHost.prepare(
                    worldPackageURL: installedRoute.initialArtifacts.packageDirectory,
                    bakeURL: installedRoute.initialArtifacts.bakeDirectory,
                    sourceContract: sourceContract
                )
                let routeRuntime = try FightboxAppleCityRouteRuntime(
                    installedRoute: installedRoute,
                    productionHost: host
                )
                guard !Task.isCancelled else {
                    _ = await host.stopAndWait()
                    lifecycleStatus = "Stopped"
                    return
                }
                try host.start(monitorGainDB: Float(monitorGainDB))
                configureAppleControl(host: host)
                appleSpatialHost = host
                appleRouteRuntime = routeRuntime
                acceptsExternalPositionControl = true
                gpsProvider?.start()
                isRunning = true
                routeQualificationAvailable = routeRuntime.plannedTransitionCount > 0
                lifecycleStatus = "Running · 48 kHz · 128 frames · Mobile"
                outputRouteStatus = host.routeSummary
                cellStreamingStatus = await routeRuntime.statusLine()
                let asset = host.playback.status()
                canonicalAssetStatus = String(
                    format: "%@ · frame %llu · %.2f MiB decoded",
                    asset.assetID,
                    asset.currentOriginalTimelineFrame,
                    Double(asset.memory.decodedResidentBytes) / 1_048_576
                )
                motionStatus = "Phone body tracking at 60 Hz · Apple owns AirPods head rotation"
                beginTelemetryPolling()
                await refreshTelemetry()
                return
            } catch {
                appleFailure = String(describing: error)
                appleSpatialHost?.stop()
                appleSpatialHost = nil
                appleRouteRuntime = nil
                routeQualificationAvailable = false
            }
        }

        do {
            try configureSteamFallback()
            guard let audioHost, let headTracker, let gpsProvider else {
                throw FightboxHostError.incompleteConfiguration
            }
            guard !Task.isCancelled else {
                lifecycleStatus = "Stopped"
                return
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
            outputRouteStatus = appleFailure.map {
                "Steam final stereo fallback · Apple route not admitted: \($0)"
            } ?? Self.initialOutputRouteStatus
            cellStreamingStatus = appleFailure.map {
                "Not active · single-world Steam fallback · strict route admission failed: \($0)"
            } ?? Self.initialCellStreamingStatus
            routeQualificationAvailable = false
            beginTelemetryPolling()
            await refreshTelemetry()
        } catch {
            audioHost?.stop()
            gpsProvider?.stop()
            headTracker?.stop()
            lifecycleStatus = "Start failed: \(error)"
            isRunning = false
        }
    }

    func runRouteQualification() {
        guard #available(iOS 18.0, *),
              isRunning,
              stopTask == nil,
              routeQualificationTask == nil,
              let routeRuntime = appleRouteRuntime
        else { return }

        routeQualificationAvailable = false
        routeQualificationInProgress = true
        acceptsExternalPositionControl = false
        gpsProvider?.stop()
        gpsStatus = "Paused · deterministic Wave 17 route owns city position"
        routeRuntime.suspendExternalPositionControl()
        motionStatus = "Phone body pose paused during exact route transitions · AirPods head rotation remains Apple-owned"
        let monitorGainToRestore = monitorGainDB
        setMonitorGainDB(-60)
        cellStreamingStatus =
            "Starting exact route qualification · monitor reduced to -60 dB · GPS and phone body pose paused"
        let transitionCount = routeRuntime.plannedTransitionCount
        routeQualificationTask = Task { [weak self, weak routeRuntime] in
            guard let self, let routeRuntime else { return }
            do {
                for offset in 0 ..< transitionCount {
                    try Task.checkCancellation()
                    guard let outcome = try await routeRuntime.advanceAuthoredNeighbor() else {
                        throw FightboxHostError.routeEndedEarly(
                            completed: offset,
                            expected: transitionCount
                        )
                    }
                    let transition = offset + 1
                    self.cellStreamingStatus =
                        "Qualification transition \(transition)/\(transitionCount) · " +
                        "\(outcome.fromCellID) → \(outcome.toCellID) · " +
                        "ownership moved exactly 1 mm"
                    await self.refreshTelemetry()
                }
                try routeRuntime.resumeBodyTracking()
                self.setMonitorGainDB(monitorGainToRestore)
                self.motionStatus =
                    "Phone body tracking resumed at 60 Hz · Apple owns AirPods head rotation"
                self.cellStreamingStatus =
                    "Exact installed route completed \(transitionCount)/\(transitionCount) transitions · " +
                    (await routeRuntime.statusLine())
                self.routeQualificationAvailable = false
            } catch is CancellationError {
                self.cellStreamingStatus = "Route qualification cancelled; no completion evidence"
                if self.stopTask == nil {
                    try? routeRuntime.resumeBodyTracking()
                    self.setMonitorGainDB(monitorGainToRestore)
                }
            } catch {
                self.cellStreamingStatus = "Route qualification failed closed: \(error)"
                if self.stopTask == nil {
                    try? routeRuntime.resumeBodyTracking()
                    self.setMonitorGainDB(monitorGainToRestore)
                    self.routeQualificationAvailable = true
                }
            }
            self.routeQualificationInProgress = false
            self.routeQualificationTask = nil
        }
    }

    @available(iOS 18.0, *)
    private func configureAppleControl(host: FightboxAppleSpatialProductionHost) {
        host.bodyMotionTracker.onError = { [weak self] error in
            Task { @MainActor in
                self?.motionStatus = "Apple body-motion error: \(error)"
            }
        }
        let gpsProvider = GpsLocalEnuProvider()
        gpsProvider.onStateChange = { [weak self, weak host] state in
            Task { @MainActor in
                guard let self, self.acceptsExternalPositionControl else { return }
                if case let .valid(reading) = state {
                    host?.setListenerPositionENU(reading.positionENU)
                }
                self.gpsStatus = Self.describeGpsState(state)
            }
        }
        self.gpsProvider = gpsProvider
    }

    private func bundledWorldURLs() throws -> (package: URL, bake: URL) {
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

        return (packageURL, bakeURL)
    }

    private func configureSteamFallback() throws {
        let urls = try bundledWorldURLs()
        let session = try FightboxSession(
            sampleRateHz: 48_000,
            blockSizeFrames: 128,
            sourceCount: 1,
            defaultSourceLevelDB: 0,
            qualityTier: FbQualityMobile,
            packageURL: urls.package,
            bakeURL: urls.bake
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
                await self?.refreshTelemetry()
            }
        }
    }

    private func refreshTelemetry() async {
        if #available(iOS 18.0, *), let appleSpatialHost {
            let asset = appleSpatialHost.playback.status()
            var routeControl = "route control unavailable"
            if let appleRouteRuntime {
                do {
                    routeControl = try await appleRouteRuntime.telemetryJSON()
                } catch {
                    routeControl = "route telemetry error: \(error)"
                }
            }
            telemetryText = """
            Apple neutral spatial
            asset: \(asset.assetID)
            artifact: \(asset.artifactID)
            frame: \(asset.currentOriginalTimelineFrame) / \(asset.frameCount)
            resident chunks: \(asset.residentChunkCount)
            underruns: \(asset.underrunCount)
            output: \(appleSpatialHost.routeSummary)
            route control: \(routeControl)
            """
            return
        }
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

    private static var initialOutputRouteStatus: String {
        if #available(iOS 18.0, *) {
            return "Steam final stereo · Apple neutral adapter installed but not promoted; target AirPods/profile/axis gate remains"
        }
        return "Steam final stereo · Apple neutral route requires iOS 18 or later"
    }

    private static var initialCellStreamingStatus: String {
        "Not active · install the exact Wave 17 route under Application Support/Fightbox/CityRoutes/active"
    }

    private static var initialCanonicalAssetStatus: String {
        "Canonical one-second chunks ready · async raw/zstd loader · 2–4 second decoded cache per source"
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
    case routeEndedEarly(completed: Int, expected: Int)

    var description: String {
        switch self {
        case let .missingBundledResource(name):
            return "Missing bundled resource \(name)"
        case .incompleteConfiguration:
            return "Fightbox host configuration is incomplete"
        case let .routeEndedEarly(completed, expected):
            return "Installed route ended after \(completed) of \(expected) transitions"
        }
    }
}
