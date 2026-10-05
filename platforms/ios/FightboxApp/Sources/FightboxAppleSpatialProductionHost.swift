import Foundation
import simd

struct FightboxAppleMacroProductionAsset: Sendable {
    let assetKey: UInt64
    let role: FightboxMacroEventRole
    let packageURL: URL
}

struct FightboxAppleMacroProductionConfiguration: Sendable {
    let diffuseProfile: FightboxMacroDiffuseProfile
    let assets: [FightboxAppleMacroProductionAsset]
}

@available(iOS 18.0, *)
final class FightboxAppleSpatialProductionHost: @unchecked Sendable {
    let sourceContract: FightboxNamedSourceContract
    let assetPackageURL: URL
    let calibrationURL: URL
    let session: FightboxNeutralSpatialSession
    let playback: FightboxCanonicalAssetPlayback
    let provider: FightboxCanonicalProgramProvider
    let adapter: AppleSpatialMixerAdapter
    let bodyMotionTracker: AppleBodyMotionTracker
    let macroLifecycleCoordinator: FightboxMacroLifecycleCoordinator

    private let shutdownLock = NSLock()
    private var shutdownTask: Task<Void, Never>?
    // A failed-closed host intentionally owns itself so its session/provider
    // cannot deinitialize while a committed token or consumed ACK is unresolved.
    private var failedClosedSelfRetention: FightboxAppleSpatialProductionHost?

    private init(
        sourceContract: FightboxNamedSourceContract,
        assetPackageURL: URL,
        calibrationURL: URL,
        session: FightboxNeutralSpatialSession,
        playback: FightboxCanonicalAssetPlayback,
        provider: FightboxCanonicalProgramProvider,
        adapter: AppleSpatialMixerAdapter,
        bodyMotionTracker: AppleBodyMotionTracker
    ) {
        self.sourceContract = sourceContract
        self.assetPackageURL = assetPackageURL
        self.calibrationURL = calibrationURL
        self.session = session
        self.playback = playback
        self.provider = provider
        self.adapter = adapter
        self.bodyMotionTracker = bodyMotionTracker
        macroLifecycleCoordinator = FightboxMacroLifecycleCoordinator(
            session: session,
            provider: provider
        )
    }

    static func prepare(
        worldPackageURL: URL,
        bakeURL: URL,
        sourceContract: FightboxNamedSourceContract,
        macroProduction: FightboxAppleMacroProductionConfiguration? = nil
    ) async throws -> FightboxAppleSpatialProductionHost {
        if let macroProduction {
            guard !macroProduction.assets.isEmpty else {
                throw FightboxAppleSpatialHostError.invalidMacroProductionConfiguration(
                    "at least one retained macro asset binding is required"
                )
            }
            guard macroProduction.assets.allSatisfy({ $0.assetKey != 0 }) else {
                throw FightboxAppleSpatialHostError.invalidMacroProductionConfiguration(
                    "macro asset keys must be nonzero"
                )
            }
            guard Set(macroProduction.assets.map(\.assetKey)).count
                    == macroProduction.assets.count,
                  Set(macroProduction.assets.map { $0.role.rawValue }).count
                    == macroProduction.assets.count
            else {
                throw FightboxAppleSpatialHostError.invalidMacroProductionConfiguration(
                    "asset keys and retained roles must each be unique"
                )
            }
        }
        let assetPackageURL = try FightboxInstalledSpatialAssets.packageURL(
            for: sourceContract
        )
        let (calibration, calibrationURL) = try FightboxInstalledSpatialAssets.axisCalibration()
        let program: FightboxSpatialSourceProgram
        switch sourceContract.expectedProvenance {
        case .nativeMono:
            program = FightboxSpatialSourceProgram(channelCount: 1, geometry: .point)
        case .authoredStereo:
            program = FightboxSpatialSourceProgram(
                channelCount: 2,
                geometry: .stereoImage(widthMeters: 2)
            )
        case .monoExpanded:
            program = FightboxSpatialSourceProgram(
                channelCount: 2,
                geometry: .stereoImage(widthMeters: 2)
            )
        }

        let programs: [FightboxSpatialSourceProgram]
        if macroProduction == nil {
            programs = [program]
        } else {
            programs = [program] + Array(
                repeating: FightboxSpatialSourceProgram(channelCount: 1, geometry: .point),
                count: 15
            )
        }
        let session = try FightboxNeutralSpatialSession(
            sampleRateHz: 48_000,
            blockSizeFrames: 128,
            programs: programs,
            environmentalOrder: 2,
            defaultSourceLevelDB: 0,
            enableMacroProductionBridge: macroProduction != nil,
            macroDiffuseProfile: macroProduction?.diffuseProfile,
            quality: .mobile,
            packageURL: worldPackageURL,
            bakeURL: bakeURL
        )
        let listenerPose = FightboxPose(
            position: .zero,
            forward: SIMD3<Float>(0, 1, 0),
            up: SIMD3<Float>(0, 0, 1)
        )
        let sourcePose = FightboxPose(
            position: SIMD3<Float>(0, 2, 0),
            forward: SIMD3<Float>(0, -1, 0),
            up: SIMD3<Float>(0, 0, 1)
        )
        let sourceStates = programs.indices.map { sourceIndex in
            FightboxSpatialSourceState(
                active: sourceIndex == 0,
                pose: sourceIndex == 0
                    ? sourcePose
                    : FightboxPose(
                        position: .zero,
                        forward: SIMD3<Float>(0, 1, 0),
                        up: SIMD3<Float>(0, 0, 1)
                    )
            )
        }
        try session.updateControlFrame(
            listenerPose: listenerPose,
            sources: sourceStates
        )
        try session.prepare()

        let playback = try await FightboxCanonicalAssetPlayback.open(
            packageURL: assetPackageURL,
            sourceIndex: 0,
            expectedProgram: program,
            cacheSeconds: 3,
            initialOriginalTimelineFrame: 0,
            active: true
        )
        guard playback.manifest.sourceAsset.assetID == sourceContract.id,
              playback.manifest.sourceAsset.layout == sourceContract.expectedLayout,
              playback.manifest.sourceAsset.presentationProvenance ==
                  sourceContract.expectedProvenance
        else {
            throw FightboxAppleSpatialHostError.sourceContractMismatch(
                expected: sourceContract.id,
                actual: playback.manifest.sourceAsset.assetID
            )
        }
        var playbacks = [playback]
        var macroBindings: [FightboxMacroAssetBinding] = []
        if let macroProduction {
            for asset in macroProduction.assets.sorted(by: { $0.role.rawValue < $1.role.rawValue }) {
                let sourceIndex = 12 + Int(asset.role.rawValue)
                let macroPlayback = try await FightboxCanonicalAssetPlayback.open(
                    packageURL: asset.packageURL,
                    sourceIndex: sourceIndex,
                    expectedProgram: programs[sourceIndex],
                    cacheSeconds: 3,
                    initialOriginalTimelineFrame: 0,
                    active: false
                )
                playbacks.append(macroPlayback)
                macroBindings.append(
                    FightboxMacroAssetBinding(
                        assetKey: asset.assetKey,
                        playback: macroPlayback
                    )
                )
            }
        }
        let provider = FightboxCanonicalProgramProvider(
            playbacks: playbacks,
            macroAssetBindings: macroBindings
        )
        let adapter = AppleSpatialMixerAdapter(
            session: session,
            programProvider: provider,
            axisCalibration: calibration
        )
        let bodyTracker = AppleBodyMotionTracker(adapter: adapter, session: session)
        return FightboxAppleSpatialProductionHost(
            sourceContract: sourceContract,
            assetPackageURL: assetPackageURL,
            calibrationURL: calibrationURL,
            session: session,
            playback: playback,
            provider: provider,
            adapter: adapter,
            bodyMotionTracker: bodyTracker
        )
    }

    func start(monitorGainDB: Float) throws {
        try adapter.start(monitorGainDB: monitorGainDB)
        do {
            try bodyMotionTracker.start(updateRateHz: 60)
        } catch {
            adapter.stop()
            throw error
        }
    }

    /// Begins serialized fail-closed shutdown without blocking the caller.
    /// `stopAndWait()` is available to owners that can await completion.
    func stop() {
        _ = beginShutdownTask()
    }

    func stopAndWait() async -> FightboxMacroLifecyclePhase {
        await beginShutdownTask().value
        let snapshot = await macroLifecycleCoordinator.snapshot()
        return snapshot.phase
    }

    private func beginShutdownTask() -> Task<Void, Never> {
        shutdownLock.lock()
        if let shutdownTask {
            shutdownLock.unlock()
            return shutdownTask
        }
        // Quiesce the control producer first, but keep the audio callback live
        // until the coordinator has finalized every terminal tail ACK.
        bodyMotionTracker.stop()
        let task = Task { [self] in
            await drainMacroLifecycleAndStopAudio()
        }
        shutdownTask = task
        shutdownLock.unlock()
        return task
    }

    private func drainMacroLifecycleAndStopAudio() async {
        // A host stop is idempotent: a second stop after a completed drain must
        // not reinterpret the already-terminal coordinator as a shutdown
        // failure and retain the host forever. The adapter is already stopped
        // whenever the coordinator reaches `.stopped`.
        if await macroLifecycleCoordinator.snapshot().phase == .stopped {
            adapter.stop()
            clearShutdownTask()
            return
        }

        var lastError: Error?
        do {
            try await macroLifecycleCoordinator.beginShutdown()
        } catch {
            await retainFailedClosedHost("shutdown quiesce failed: \(error)")
            return
        }
        let initial = await macroLifecycleCoordinator.snapshot()
        let tailSpanFrames: UInt64
        if let effective = initial.committedEffectiveFrame,
           let tail = initial.committedTailDeadlineFrame,
           tail >= effective
        {
            tailSpanFrames = tail - effective
        } else {
            tailSpanFrames = 0
        }
        // Tail deadlines are advisory maxima. Terminal ACK finalization remains
        // the completion truth, with five seconds of control-side retry margin.
        let retryBudgetSeconds = min(
            600.0,
            max(5.0, Double(tailSpanFrames) / 48_000.0 + 5.0)
        )
        let watchdogDeadline = ProcessInfo.processInfo.systemUptime + retryBudgetSeconds

        while true {
            do {
                _ = try await macroLifecycleCoordinator.drainAvailableAcknowledgements()
                lastError = nil
            } catch {
                // A consumed ACK remains retained inside the coordinator. Retry
                // provider release or Rust finalization without polling again.
                lastError = error
                let snapshot = await macroLifecycleCoordinator.snapshot()
                if snapshot.phase == .failedClosed {
                    await retainFailedClosedHost("macro ACK drain failed closed: \(error)")
                    return
                }
            }
            if await macroLifecycleCoordinator.drainIsComplete() {
                adapter.stop()
                do {
                    try await macroLifecycleCoordinator.markStopped()
                    clearShutdownTask()
                    return
                } catch {
                    await retainFailedClosedHost("audio stopped before lifecycle finalization: \(error)")
                    return
                }
            }
            if ProcessInfo.processInfo.systemUptime >= watchdogDeadline {
                let suffix = lastError.map { "; last retry error=\($0)" } ?? ""
                await retainFailedClosedHost(
                    "macro terminal ACK watchdog expired after \(retryBudgetSeconds)s\(suffix)"
                )
                return
            }
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
    }

    private func retainFailedClosedHost(_ detail: String) async {
        await macroLifecycleCoordinator.failClosed(detail)
        storeFailedClosedRetention()
        // Deliberately do not stop the callback or release the session. This is
        // the fail-closed state; explicit process teardown is the recovery path.
    }

    private func storeFailedClosedRetention() {
        shutdownLock.lock()
        failedClosedSelfRetention = self
        shutdownTask = nil
        shutdownLock.unlock()
    }

    private func clearShutdownTask() {
        shutdownLock.lock()
        shutdownTask = nil
        shutdownLock.unlock()
    }

    func setMonitorGainDB(_ gainDB: Float) throws {
        try adapter.setMonitorGainDB(gainDB)
    }

    func setListenerPositionENU(_ position: SIMD3<Float>) {
        bodyMotionTracker.listenerPositionENU = position
    }

    func admitMacroEvents(_ events: [FightboxMacroEventAdmission]) throws {
        try session.admitMacroEvents(events)
    }

    func bindMacroEchoAnchor(
        eventID: UInt64,
        anchorKey: FightboxStableSpatialKey
    ) throws {
        try session.bindMacroEchoAnchor(eventID: eventID, anchorKey: anchorKey)
    }

    /// Performs exact provider preparation and all-or-nothing Ready staging on
    /// the serialized lifecycle actor. Pre-commit failure rolls provider state
    /// back before discarding the Rust token; rollback failure is failed closed.
    func prepareMacroToken(
        lookaheadFrame: UInt64
    ) async throws -> FightboxMacroTokenPreparation? {
        do {
            return try await macroLifecycleCoordinator.prepare(lookaheadFrame: lookaheadFrame)
        } catch {
            if (await macroLifecycleCoordinator.snapshot()).phase == .failedClosed {
                await retainFailedClosedHost("macro preparation failed closed: \(error)")
            }
            throw error
        }
    }

    /// Commits the exact control frame, then registers its authoritative V3
    /// result. Status 3 is committed even when `ffiResultRawValue` is nonzero.
    func commitMacroToken(
        _ tokenID: UInt64,
        listenerPose: FightboxPose,
        listenerLinearVelocityMPS: SIMD3<Float> = .zero,
        sources: [FightboxSpatialSourceState]
    ) async throws -> FightboxMacroCommitResult {
        let result = try session.commitMacroControlFrame(
            tokenID: tokenID,
            listenerPose: listenerPose,
            listenerLinearVelocityMPS: listenerLinearVelocityMPS,
            sources: sources
        )
        do {
            return try await macroLifecycleCoordinator.registerCommit(result)
        } catch {
            if result.status == 3 {
                await retainFailedClosedHost(
                    "authoritative committed result failed host validation: \(error)"
                )
            }
            throw error
        }
    }

    /// Retries retained consumed ACKs without repolling. Exact provider release
    /// completes before Rust finalization and is never repeated after success.
    @discardableResult
    func finalizeAvailableMacroAcknowledgements() async throws -> Int {
        do {
            return try await macroLifecycleCoordinator.drainAvailableAcknowledgements()
        } catch {
            if (await macroLifecycleCoordinator.snapshot()).phase == .failedClosed {
                await retainFailedClosedHost("macro acknowledgement failed closed: \(error)")
            }
            throw error
        }
    }

    /// Control-side ingress hook. Rust macro transport owns the value; this
    /// host applies it without recomputing distance or propagation delay.
    func applyMacroArrival(programSeekFrame: UInt64) async throws {
        guard !session.macroProductionBridgeEnabled else {
            throw FightboxAppleSpatialHostError.invalidMacroProductionConfiguration(
                "tokened macro production must use prepareMacroToken, not the legacy source seek"
            )
        }
        try await playback.seekForMacroArrival(
            FightboxMacroAssetArrival(programSeekFrame: programSeekFrame)
        )
    }

    var routeSummary: String {
        let status = adapter.status
        return "Apple spatial · \(sourceContract.displayName) · " +
            "axis \(status.axisCalibrationIdentifier) · " +
            "head \(status.automaticHeadTracking) · profile \(status.personalizedHRTF)"
    }
}

private enum FightboxInstalledSpatialAssets {
    private static let audioPackageExtension = "fightbox-audio"
    private static let calibrationName = "apple-spatial-axis-calibration"

    static func packageURL(for contract: FightboxNamedSourceContract) throws -> URL {
        let candidates = applicationSupportRoots().map {
            $0.appendingPathComponent("CanonicalAssets", isDirectory: true)
                .appendingPathComponent(
                    "\(contract.id).\(audioPackageExtension)",
                    isDirectory: true
                )
        } + [Bundle.main.url(forResource: contract.id, withExtension: audioPackageExtension)]
            .compactMap { $0 }
        guard let url = candidates.first(where: isDirectory) else {
            throw FightboxAppleSpatialHostError.missingCanonicalPackage(
                "\(contract.id).\(audioPackageExtension)"
            )
        }
        return url
    }

    static func axisCalibration() throws -> (AppleSpatialAxisCalibration, URL) {
        let candidates = applicationSupportRoots().map {
            $0.appendingPathComponent("\(calibrationName).json")
        } + [Bundle.main.url(forResource: calibrationName, withExtension: "json")]
            .compactMap { $0 }
        guard let url = candidates.first(where: { FileManager.default.fileExists(atPath: $0.path) })
        else {
            throw FightboxAppleSpatialHostError.missingAxisCalibration
        }
        let document: FightboxAppleAxisCalibrationDocument
        do {
            document = try JSONDecoder().decode(
                FightboxAppleAxisCalibrationDocument.self,
                from: Data(contentsOf: url)
            )
        } catch {
            throw FightboxAppleSpatialHostError.invalidAxisCalibration(
                error.localizedDescription
            )
        }
        return (try document.makeCalibration(), url)
    }

    private static func applicationSupportRoots() -> [URL] {
        FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        ).map { $0.appendingPathComponent("Fightbox", isDirectory: true) }
    }

    private static func isDirectory(_ url: URL) -> Bool {
        var directory: ObjCBool = false
        return FileManager.default.fileExists(atPath: url.path, isDirectory: &directory)
            && directory.boolValue
    }
}

private struct FightboxAppleAxisCalibrationDocument: Decodable {
    let schemaVersion: String
    let rightENU: [Float]
    let frontENU: [Float]
    let upENU: [Float]
    let environmentalAcnTransform: [Float]
    let expectedEnvironmentalBasis: String
    let evidenceIdentifier: String
    let targetDeviceVerified: Bool

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case rightENU = "right_enu"
        case frontENU = "front_enu"
        case upENU = "up_enu"
        case environmentalAcnTransform = "environmental_acn_transform"
        case expectedEnvironmentalBasis = "expected_environmental_basis"
        case evidenceIdentifier = "evidence_identifier"
        case targetDeviceVerified = "target_device_verified"
    }

    func makeCalibration() throws -> AppleSpatialAxisCalibration {
        guard schemaVersion == "fightbox.apple-spatial-axis-calibration.v1",
              rightENU.count == 3,
              frontENU.count == 3,
              upENU.count == 3,
              environmentalAcnTransform.count == 81,
              (rightENU + frontENU + upENU + environmentalAcnTransform).allSatisfy(\.isFinite),
              !evidenceIdentifier.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              targetDeviceVerified
        else {
            throw FightboxAppleSpatialHostError.invalidAxisCalibration(
                "shape, finite-value, evidence, or target-device verification failed"
            )
        }
        let right = SIMD3<Float>(rightENU[0], rightENU[1], rightENU[2])
        let front = SIMD3<Float>(frontENU[0], frontENU[1], frontENU[2])
        let up = SIMD3<Float>(upENU[0], upENU[1], upENU[2])
        guard abs(simd_length(right) - 1) < 0.001,
              abs(simd_length(front) - 1) < 0.001,
              abs(simd_length(up) - 1) < 0.001,
              abs(simd_dot(right, front)) < 0.001,
              abs(simd_dot(right, up)) < 0.001,
              abs(simd_dot(front, up)) < 0.001
        else {
            throw FightboxAppleSpatialHostError.invalidAxisCalibration(
                "right/front/up axes must be an orthonormal basis"
            )
        }
        let basis: AppleSpatialAxisCalibration.EnvironmentalBasis
        switch expectedEnvironmentalBasis {
        case "right_handed_enu": basis = .rightHandedENU
        case "steam_x_right_y_up_z_back": basis = .steamXRightYUpZBack
        default:
            throw FightboxAppleSpatialHostError.invalidAxisCalibration(
                "unknown environmental basis \(expectedEnvironmentalBasis)"
            )
        }
        return AppleSpatialAxisCalibration(
            rightENU: right,
            frontENU: front,
            upENU: up,
            environmentalAcnTransform: environmentalAcnTransform,
            expectedEnvironmentalBasis: basis,
            evidenceIdentifier: evidenceIdentifier,
            isTargetDeviceVerified: true
        )
    }
}

enum FightboxAppleSpatialHostError: Error, CustomStringConvertible {
    case missingCanonicalPackage(String)
    case missingAxisCalibration
    case invalidAxisCalibration(String)
    case sourceContractMismatch(expected: String, actual: String)
    case invalidMacroProductionConfiguration(String)
    case macroProductionRollbackFailed(original: String, cleanup: String)

    var description: String {
        switch self {
        case let .missingCanonicalPackage(name):
            return "Install licensed canonical package \(name) in Application Support/Fightbox/CanonicalAssets"
        case .missingAxisCalibration:
            return "Install target-device apple-spatial-axis-calibration.json in Application Support/Fightbox"
        case let .invalidAxisCalibration(reason):
            return "Invalid Apple spatial axis calibration: \(reason)"
        case let .sourceContractMismatch(expected, actual):
            return "Selected source contract \(expected) does not match installed package \(actual)"
        case let .invalidMacroProductionConfiguration(reason):
            return "Invalid macro production configuration: \(reason)"
        case let .macroProductionRollbackFailed(original, cleanup):
            return "Macro preparation failed (\(original)); rollback also failed: \(cleanup)"
        }
    }
}
