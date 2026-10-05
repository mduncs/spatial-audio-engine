import FightboxC
import FightboxKit
import Foundation

enum HarnessError: Error, CustomStringConvertible {
    case check(String)
    case injected(String)

    var description: String {
        switch self {
        case let .check(message), let .injected(message): message
        }
    }
}

func check(_ condition: @autoclosure () -> Bool, _ message: String) throws {
    if !condition() { throw HarnessError.check(message) }
}

final class MockSession: FightboxMacroLifecycleSession, @unchecked Sendable {
    var nextPreparation: FightboxMacroTokenPreparation?
    var staged: [[FightboxMacroPreparedAsset]] = []
    var discarded: [UInt64] = []
    var acknowledgements: [FightboxMacroAudioAcknowledgement] = []
    var pollCount = 0
    var finalized: [FightboxMacroAudioAcknowledgement] = []
    var finalizeFailuresRemaining = 0
    var discardFails = false
    var destroyCount = 0

    func prepareMacroToken(lookaheadFrame: UInt64) throws -> FightboxMacroTokenPreparation? {
        nextPreparation
    }

    func stageMacroReady(_ assets: [FightboxMacroPreparedAsset]) throws {
        staged.append(assets)
    }

    func discardMacroToken(_ tokenID: UInt64) throws {
        if discardFails { throw HarnessError.injected("discard") }
        discarded.append(tokenID)
    }

    func pollMacroAcknowledgement() throws -> FightboxMacroAudioAcknowledgement? {
        pollCount += 1
        guard !acknowledgements.isEmpty else { return nil }
        return acknowledgements.removeFirst()
    }

    func finalizeMacroAcknowledgement(
        _ acknowledgement: FightboxMacroAudioAcknowledgement
    ) throws {
        if finalizeFailuresRemaining > 0 {
            finalizeFailuresRemaining -= 1
            throw HarnessError.injected("finalize")
        }
        finalized.append(acknowledgement)
    }
}

final class MockProvider: FightboxMacroLifecycleProvider, @unchecked Sendable {
    var sourceByAsset: [UInt64: Int] = [:]
    var generationByAsset: [UInt64: UInt64] = [:]
    var prepareFailureAsset: UInt64?
    var releaseFailuresRemaining: [UInt64: Int] = [:]
    var prepareDelayNanoseconds: UInt64 = 0
    var prepareCalls: [(UInt64, UInt64)] = []
    var releaseCalls: [FightboxMacroAssetReadiness] = []

    func prepareMacroAsset(
        assetKey: UInt64,
        programSeekFrame: UInt64
    ) async throws -> FightboxMacroAssetReadiness {
        prepareCalls.append((assetKey, programSeekFrame))
        if prepareDelayNanoseconds > 0 {
            try await Task.sleep(nanoseconds: prepareDelayNanoseconds)
        }
        if prepareFailureAsset == assetKey {
            throw HarnessError.injected("prepare \(assetKey)")
        }
        return FightboxMacroAssetReadiness(
            assetKey: assetKey,
            sourceIndex: sourceByAsset[assetKey] ?? Int(assetKey % 16),
            programSeekFrame: programSeekFrame,
            discontinuitySequence: generationByAsset[assetKey] ?? 2
        )
    }

    func releaseMacroAsset(_ readiness: FightboxMacroAssetReadiness) -> Bool {
        releaseCalls.append(readiness)
        if let remaining = releaseFailuresRemaining[readiness.assetKey], remaining > 0 {
            releaseFailuresRemaining[readiness.assetKey] = remaining - 1
            return false
        }
        return generationByAsset[readiness.assetKey] == readiness.discontinuitySequence
            && sourceByAsset[readiness.assetKey] == readiness.sourceIndex
    }
}

func event(
    id: UInt64,
    role: UInt32,
    asset: UInt64,
    seek: UInt64,
    activation: UInt64 = 1_024,
    tail: UInt64 = 8_192
) -> FightboxMacroTokenEvent {
    FightboxMacroTokenEvent(
        eventID: id,
        atomicGroupID: 900,
        role: role,
        assetKey: asset,
        activationFrame: activation,
        programSeekFrame: seek,
        tailDeadlineFrame: tail
    )
}

func committed(
    token: UInt64,
    count: Int,
    ffi: Int32 = -7,
    generation: UInt64 = 44,
    effective: UInt64 = 1_024,
    tail: UInt64 = 10_240
) -> FightboxMacroCommitResult {
    FightboxMacroCommitResult(
        ffiResultRawValue: ffi,
        tokenID: token,
        status: 3,
        eventCount: count,
        directGeneration: generation,
        effectiveFrame: effective,
        tailDeadlineFrame: tail
    )
}

func acknowledgement(
    token: UInt64,
    event: FightboxMacroTokenEvent,
    readiness: FightboxMacroAssetReadiness,
    generation: UInt64 = 44,
    effective: UInt64 = 1_024,
    status: UInt32 = 0
) -> FightboxMacroAudioAcknowledgement {
    FightboxMacroAudioAcknowledgement(
        tokenID: token,
        eventID: event.eventID,
        role: event.role,
        status: status,
        readiness: readiness,
        directGeneration: generation,
        effectiveFrame: effective
    )
}

func exactCommitAndConsumedAckRetry() async throws {
    let first = event(id: 11, role: 0, asset: 101, seek: 7)
    let second = event(id: 12, role: 1, asset: 102, seek: 9)
    let preparation = FightboxMacroTokenPreparation(
        tokenID: 77,
        lookaheadFrame: 512,
        events: [first, second]
    )
    let session = MockSession()
    session.nextPreparation = preparation
    let provider = MockProvider()
    provider.sourceByAsset = [101: 12, 102: 13]
    provider.generationByAsset = [101: 2, 102: 4]
    let coordinator = FightboxMacroLifecycleCoordinator(session: session, provider: provider)

    let prepared = try await coordinator.prepare(lookaheadFrame: 512)
    try check(prepared == preparation, "prepare")
    try check(session.staged.count == 1 && session.staged[0].count == 2, "stage")
    let result = committed(token: 77, count: 2)
    _ = try await coordinator.registerCommit(result)
    let committedSnapshot = await coordinator.snapshot()
    try check(committedSnapshot.phase == .committed, "ffi error + status 3 must commit")
    try check(committedSnapshot.committedTailDeadlineFrame == 10_240, "tail deadline")

    let readies = session.staged[0].map(\.readiness)
    session.acknowledgements = [
        acknowledgement(token: 77, event: first, readiness: readies[0]),
        acknowledgement(token: 77, event: second, readiness: readies[1], status: 1),
    ]
    provider.releaseFailuresRemaining[101] = 1
    try await coordinator.beginShutdown()
    do {
        _ = try await coordinator.drainAvailableAcknowledgements()
        throw HarnessError.check("provider release failure was not surfaced")
    } catch FightboxMacroLifecycleError.providerReleaseRejected(eventID: 11) {}
    let retained = await coordinator.snapshot()
    try check(retained.hasConsumedAcknowledgement, "consumed ACK must be retained")
    try check(!retained.providerReleaseCompletedForConsumedAcknowledgement, "release stage")
    try check(session.pollCount == 1, "must not repoll after consumed ACK")

    session.finalizeFailuresRemaining = 1
    do {
        _ = try await coordinator.drainAvailableAcknowledgements()
        throw HarnessError.check("finalize failure was not surfaced")
    } catch HarnessError.injected("finalize") {}
    let awaitingFinalize = await coordinator.snapshot()
    try check(awaitingFinalize.providerReleaseCompletedForConsumedAcknowledgement, "finalize stage")
    try check(provider.releaseCalls.filter { $0.assetKey == 101 }.count == 2, "one failed + one successful release")
    try check(session.pollCount == 1, "finalize retry must not poll")

    let finalized = try await coordinator.drainAvailableAcknowledgements()
    try check(finalized == 2, "both terminal ACKs finalized")
    try check(provider.releaseCalls.filter { $0.assetKey == 101 }.count == 2, "release must not repeat after success")
    let drainComplete = await coordinator.drainIsComplete()
    try check(drainComplete, "drain complete")
    try await coordinator.markStopped()
    let stoppedSnapshot = await coordinator.snapshot()
    try check(stoppedSnapshot.phase == .stopped, "stopped")
}

func directFailureRemainsReadyAndRollsBack() async throws {
    let only = event(id: 21, role: 2, asset: 201, seek: 33)
    let session = MockSession()
    session.nextPreparation = FightboxMacroTokenPreparation(
        tokenID: 88,
        lookaheadFrame: 700,
        events: [only]
    )
    let provider = MockProvider()
    provider.sourceByAsset = [201: 14]
    provider.generationByAsset = [201: 6]
    let coordinator = FightboxMacroLifecycleCoordinator(session: session, provider: provider)
    _ = try await coordinator.prepare(lookaheadFrame: 700)
    let notCommitted = FightboxMacroCommitResult(
        ffiResultRawValue: -3,
        tokenID: 88,
        status: 2,
        eventCount: 1,
        directGeneration: 0,
        effectiveFrame: 1_024,
        tailDeadlineFrame: 0
    )
    do {
        _ = try await coordinator.registerCommit(notCommitted)
        throw HarnessError.check("non-3 commit accepted")
    } catch FightboxMacroLifecycleError.commitNotTerminallyCommitted(status: 2, ffiResult: -3) {}
    let readySnapshot = await coordinator.snapshot()
    try check(readySnapshot.phase == .ready, "direct failure remains Ready")
    try await coordinator.beginShutdown()
    try check(session.discarded == [88], "precommit token discarded")
    try check(provider.releaseCalls.count == 1, "precommit generation released")
    let rollbackDrainComplete = await coordinator.drainIsComplete()
    try check(rollbackDrainComplete, "ready rollback drains")
}

func partialPrepareRollbackAndFailClosed() async throws {
    let events = [
        event(id: 31, role: 0, asset: 301, seek: 1),
        event(id: 32, role: 1, asset: 302, seek: 2),
    ]
    let session = MockSession()
    session.nextPreparation = FightboxMacroTokenPreparation(
        tokenID: 99,
        lookaheadFrame: 800,
        events: events
    )
    let provider = MockProvider()
    provider.sourceByAsset = [301: 12, 302: 13]
    provider.generationByAsset = [301: 8, 302: 10]
    provider.prepareFailureAsset = 302
    let coordinator = FightboxMacroLifecycleCoordinator(session: session, provider: provider)
    do {
        _ = try await coordinator.prepare(lookaheadFrame: 800)
        throw HarnessError.check("partial prepare unexpectedly passed")
    } catch HarnessError.injected("prepare 302") {}
    try check(provider.releaseCalls.map(\.assetKey) == [301], "partial readiness rollback")
    try check(session.discarded == [99], "partial token discard")
    let runningSnapshot = await coordinator.snapshot()
    try check(runningSnapshot.phase == .running, "rollback returns running")

    provider.prepareFailureAsset = 302
    provider.releaseFailuresRemaining[301] = 1
    session.nextPreparation = FightboxMacroTokenPreparation(
        tokenID: 100,
        lookaheadFrame: 801,
        events: events
    )
    do {
        _ = try await coordinator.prepare(lookaheadFrame: 801)
        throw HarnessError.check("rollback failure unexpectedly passed")
    } catch FightboxMacroLifecycleError.rollbackFailed {}
    let failedRollbackSnapshot = await coordinator.snapshot()
    try check(failedRollbackSnapshot.phase == .failedClosed, "rollback fail closed")
    try check(session.discarded == [99], "live provider generation retains Rust token")
    try check(session.destroyCount == 0, "failed-closed session retained")
}

func staleGenerationAckIsRetained() async throws {
    let only = event(id: 41, role: 0, asset: 401, seek: 5)
    let session = MockSession()
    session.nextPreparation = FightboxMacroTokenPreparation(
        tokenID: 111,
        lookaheadFrame: 900,
        events: [only]
    )
    let provider = MockProvider()
    provider.sourceByAsset = [401: 12]
    provider.generationByAsset = [401: 12]
    let coordinator = FightboxMacroLifecycleCoordinator(session: session, provider: provider)
    _ = try await coordinator.prepare(lookaheadFrame: 900)
    _ = try await coordinator.registerCommit(committed(token: 111, count: 1))
    let readiness = session.staged[0][0].readiness
    session.acknowledgements = [acknowledgement(token: 111, event: only, readiness: readiness)]
    provider.generationByAsset[401] = 14 // successor now owns the slot
    try await coordinator.beginShutdown()
    do {
        _ = try await coordinator.drainAvailableAcknowledgements()
        throw HarnessError.check("stale generation release unexpectedly passed")
    } catch FightboxMacroLifecycleError.providerReleaseRejected(eventID: 41) {}
    let snapshot = await coordinator.snapshot()
    try check(snapshot.hasConsumedAcknowledgement, "stale ACK retained")
    try check(session.finalized.isEmpty, "stale ACK not finalized")
    await coordinator.failClosed("stale successor generation")
    let failedClosedSnapshot = await coordinator.snapshot()
    try check(failedClosedSnapshot.phase == .failedClosed, "explicit fail closed")
    try check(session.destroyCount == 0, "failed-closed session retained")
}


func shutdownWhileProviderPrepareIsSuspended() async throws {
    let only = event(id: 51, role: 0, asset: 501, seek: 17)
    let session = MockSession()
    session.nextPreparation = FightboxMacroTokenPreparation(
        tokenID: 121,
        lookaheadFrame: 1_000,
        events: [only]
    )
    let provider = MockProvider()
    provider.sourceByAsset = [501: 12]
    provider.generationByAsset = [501: 16]
    provider.prepareDelayNanoseconds = 50_000_000
    let coordinator = FightboxMacroLifecycleCoordinator(session: session, provider: provider)
    let prepareTask = Task {
        try await coordinator.prepare(lookaheadFrame: 1_000)
    }
    try await Task.sleep(nanoseconds: 5_000_000)
    try await coordinator.beginShutdown()
    do {
        _ = try await prepareTask.value
        throw HarnessError.check("quiesced preparation unexpectedly passed")
    } catch FightboxMacroLifecycleError.readinessIdentityMismatch(eventID: 51) {}
    let snapshot = await coordinator.snapshot()
    try check(snapshot.phase == .draining, "quiesced prepare rolls into draining")
    try check(provider.releaseCalls.map(\.assetKey) == [501], "quiesced generation released")
    try check(session.discarded == [121], "quiesced token discarded")
    let complete = await coordinator.drainIsComplete()
    try check(complete, "quiesced preparation fully drained")
}



func invalidConsumedAcknowledgementFailsClosedWithoutRepoll() async throws {
    let only = event(id: 61, role: 0, asset: 601, seek: 23)
    let session = MockSession()
    session.nextPreparation = FightboxMacroTokenPreparation(
        tokenID: 131,
        lookaheadFrame: 1_100,
        events: [only]
    )
    let provider = MockProvider()
    provider.sourceByAsset = [601: 12]
    provider.generationByAsset = [601: 18]
    let coordinator = FightboxMacroLifecycleCoordinator(session: session, provider: provider)
    _ = try await coordinator.prepare(lookaheadFrame: 1_100)
    _ = try await coordinator.registerCommit(committed(token: 131, count: 1))
    var wrongReadiness = session.staged[0][0].readiness
    wrongReadiness = FightboxMacroAssetReadiness(
        assetKey: wrongReadiness.assetKey,
        sourceIndex: wrongReadiness.sourceIndex,
        programSeekFrame: wrongReadiness.programSeekFrame + 1,
        discontinuitySequence: wrongReadiness.discontinuitySequence
    )
    session.acknowledgements = [
        acknowledgement(token: 131, event: only, readiness: wrongReadiness)
    ]
    try await coordinator.beginShutdown()
    do {
        _ = try await coordinator.drainAvailableAcknowledgements()
        throw HarnessError.check("invalid consumed ACK unexpectedly passed")
    } catch FightboxMacroLifecycleError.acknowledgementIdentityMismatch {}
    let snapshot = await coordinator.snapshot()
    try check(snapshot.phase == .failedClosed, "invalid consumed ACK fails closed")
    try check(snapshot.hasConsumedAcknowledgement, "invalid consumed ACK retained")
    try check(session.pollCount == 1, "invalid consumed ACK polled once")
    do {
        _ = try await coordinator.drainAvailableAcknowledgements()
        throw HarnessError.check("failed-closed drain unexpectedly retried")
    } catch FightboxMacroLifecycleError.notRunning(.failedClosed) {}
    try check(session.pollCount == 1, "failed-closed ACK never repolled")
    try check(provider.releaseCalls.isEmpty, "invalid ACK cannot release provider")
}

func failClosedWhileProviderPrepareIsSuspendedStaysFailedClosed() async throws {
    let only = event(id: 71, role: 0, asset: 701, seek: 29)
    let session = MockSession()
    session.nextPreparation = FightboxMacroTokenPreparation(
        tokenID: 141,
        lookaheadFrame: 1_200,
        events: [only]
    )
    let provider = MockProvider()
    provider.sourceByAsset = [701: 12]
    provider.generationByAsset = [701: 20]
    provider.prepareDelayNanoseconds = 50_000_000
    let coordinator = FightboxMacroLifecycleCoordinator(session: session, provider: provider)
    let prepareTask = Task {
        try await coordinator.prepare(lookaheadFrame: 1_200)
    }
    try await Task.sleep(nanoseconds: 5_000_000)
    await coordinator.failClosed("injected watchdog")
    do {
        _ = try await prepareTask.value
        throw HarnessError.check("failed-closed suspended prepare unexpectedly passed")
    } catch FightboxMacroLifecycleError.readinessIdentityMismatch(eventID: 71) {}
    let snapshot = await coordinator.snapshot()
    try check(snapshot.phase == .failedClosed, "rollback must preserve failed closed")
    try check(provider.releaseCalls.map(\.assetKey) == [701], "failed-closed readiness released")
    try check(session.discarded == [141], "failed-closed precommit token cleaned exactly")
}

func v3CommitABILayout() throws {
    try check(MemoryLayout<FbMacroCommitResultV3>.size == 64, "V3 commit ABI size")
    try check(MemoryLayout<FbMacroCommitResultV3>.stride == 64, "V3 commit ABI stride")
    try check(
        MemoryLayout<FbMacroCommitResultV3>.offset(of: \.tail_deadline_frame) == 40,
        "V3 tail deadline ABI offset"
    )
}

@main
struct FightboxMacroLifecycleHarness {
    static func main() async throws {
        try await exactCommitAndConsumedAckRetry()
        try await directFailureRemainsReadyAndRollsBack()
        try await partialPrepareRollbackAndFailClosed()
        try await staleGenerationAckIsRetained()
        try await shutdownWhileProviderPrepareIsSuspended()
        try await invalidConsumedAcknowledgementFailsClosedWithoutRepoll()
        try await failClosedWhileProviderPrepareIsSuspendedStaysFailedClosed()
        try v3CommitABILayout()
        print("FightboxMacroLifecycleHarness PASS scenarios=8")
    }
}
