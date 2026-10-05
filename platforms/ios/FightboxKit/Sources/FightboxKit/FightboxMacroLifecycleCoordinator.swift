import Foundation

/// Minimal session seam required by the host-owned V3 lifecycle coordinator.
public protocol FightboxMacroLifecycleSession: AnyObject, Sendable {
    func prepareMacroToken(lookaheadFrame: UInt64) throws -> FightboxMacroTokenPreparation?
    func stageMacroReady(_ assets: [FightboxMacroPreparedAsset]) throws
    func discardMacroToken(_ tokenID: UInt64) throws
    func pollMacroAcknowledgement() throws -> FightboxMacroAudioAcknowledgement?
    func finalizeMacroAcknowledgement(_ acknowledgement: FightboxMacroAudioAcknowledgement) throws
}

extension FightboxNeutralSpatialSession: FightboxMacroLifecycleSession {}

/// Exact generation-qualified provider seam used before commit and after terminal ACK.
public protocol FightboxMacroLifecycleProvider: AnyObject, Sendable {
    func prepareMacroAsset(
        assetKey: UInt64,
        programSeekFrame: UInt64
    ) async throws -> FightboxMacroAssetReadiness

    @discardableResult
    func releaseMacroAsset(_ readiness: FightboxMacroAssetReadiness) -> Bool
}

@available(macOS 10.15, *)
extension FightboxCanonicalProgramProvider: FightboxMacroLifecycleProvider {}

public enum FightboxMacroLifecyclePhase: String, Sendable, Equatable {
    case running
    case preparing
    case ready
    case committed
    case quiescing
    case draining
    case stopped
    case failedClosed
}

public struct FightboxMacroLifecycleSnapshot: Sendable, Equatable {
    public let phase: FightboxMacroLifecyclePhase
    public let tokenID: UInt64?
    public let preparedAssetCount: Int
    public let remainingAcknowledgementCount: Int
    public let hasConsumedAcknowledgement: Bool
    public let providerReleaseCompletedForConsumedAcknowledgement: Bool
    public let committedEffectiveFrame: UInt64?
    public let committedTailDeadlineFrame: UInt64?
}

public enum FightboxMacroLifecycleError: Error, Sendable, Equatable, CustomStringConvertible {
    case notRunning(FightboxMacroLifecyclePhase)
    case transactionAlreadyActive
    case noReadyToken
    case commitNotTerminallyCommitted(status: UInt32, ffiResult: Int32)
    case commitIdentityMismatch(String)
    case readinessIdentityMismatch(eventID: UInt64)
    case rollbackFailed(String)
    case acknowledgementIdentityMismatch(String)
    case providerReleaseRejected(eventID: UInt64)
    case drainNotComplete
    case failedClosed(String)

    public var description: String {
        switch self {
        case let .notRunning(phase):
            "macro lifecycle is not running (\(phase.rawValue))"
        case .transactionAlreadyActive:
            "a macro lifecycle transaction is already active"
        case .noReadyToken:
            "no Ready macro token exists"
        case let .commitNotTerminallyCommitted(status, ffi):
            "macro commit did not reach status 3 (status=\(status), ffi=\(ffi))"
        case let .commitIdentityMismatch(detail):
            "macro commit identity mismatch: \(detail)"
        case let .readinessIdentityMismatch(eventID):
            "macro readiness identity mismatch for event \(eventID)"
        case let .rollbackFailed(detail):
            "macro pre-commit rollback failed closed: \(detail)"
        case let .acknowledgementIdentityMismatch(detail):
            "macro acknowledgement identity mismatch: \(detail)"
        case let .providerReleaseRejected(eventID):
            "provider rejected exact acknowledged generation for event \(eventID)"
        case .drainNotComplete:
            "macro lifecycle drain is not complete"
        case let .failedClosed(detail):
            "macro lifecycle failed closed: \(detail)"
        }
    }
}

/// Serial, fail-closed owner for one exact V3 token transaction.
///
/// Polling an ACK consumes the Rust mailbox entry, so `consumedAcknowledgement`
/// is retained across provider-release and Rust-finalize retries. A status-3
/// commit remains committed even when its optional-path FFI result is nonzero.
public actor FightboxMacroLifecycleCoordinator {
    private enum ConsumedStage: Sendable {
        case invalidIdentity
        case awaitingProviderRelease
        case awaitingRustFinalize
    }

    private struct Consumed: Sendable {
        let acknowledgement: FightboxMacroAudioAcknowledgement
        var stage: ConsumedStage
    }

    private let session: any FightboxMacroLifecycleSession
    private let provider: any FightboxMacroLifecycleProvider
    private var phase: FightboxMacroLifecyclePhase = .running
    private var preparation: FightboxMacroTokenPreparation?
    private var preparedAssets: [FightboxMacroPreparedAsset] = []
    private var commit: FightboxMacroCommitResult?
    private var remainingEventIDs: Set<UInt64> = []
    private var consumedAcknowledgement: Consumed?
    private var failureDetail: String?
    private var shutdownRequested = false

    public init(
        session: any FightboxMacroLifecycleSession,
        provider: any FightboxMacroLifecycleProvider
    ) {
        self.session = session
        self.provider = provider
    }

    public func snapshot() -> FightboxMacroLifecycleSnapshot {
        FightboxMacroLifecycleSnapshot(
            phase: phase,
            tokenID: preparation?.tokenID,
            preparedAssetCount: preparedAssets.count,
            remainingAcknowledgementCount: remainingEventIDs.count,
            hasConsumedAcknowledgement: consumedAcknowledgement != nil,
            providerReleaseCompletedForConsumedAcknowledgement:
                consumedAcknowledgement?.stage == .awaitingRustFinalize,
            committedEffectiveFrame: commit?.effectiveFrame,
            committedTailDeadlineFrame: commit?.tailDeadlineFrame
        )
    }

    public func prepare(
        lookaheadFrame: UInt64
    ) async throws -> FightboxMacroTokenPreparation? {
        guard phase == .running else { throw FightboxMacroLifecycleError.notRunning(phase) }
        guard preparation == nil, consumedAcknowledgement == nil else {
            throw FightboxMacroLifecycleError.transactionAlreadyActive
        }
        guard let next = try session.prepareMacroToken(lookaheadFrame: lookaheadFrame) else {
            return nil
        }
        preparation = next
        preparedAssets = []
        phase = .preparing
        do {
            for event in next.events {
                let readiness = try await provider.prepareMacroAsset(
                    assetKey: event.assetKey,
                    programSeekFrame: event.programSeekFrame
                )
                // The provider has activated this exact generation already.
                // Retain it before any validation so shutdown or hostile
                // readiness failure can release it during rollback.
                preparedAssets.append(
                    FightboxMacroPreparedAsset(
                        tokenID: next.tokenID,
                        eventID: event.eventID,
                        role: event.role,
                        readiness: readiness
                    )
                )
                guard phase == .preparing,
                      readiness.assetKey == event.assetKey,
                      readiness.programSeekFrame == event.programSeekFrame,
                      readiness.discontinuitySequence != 0,
                      readiness.discontinuitySequence & 1 == 0
                else {
                    throw FightboxMacroLifecycleError.readinessIdentityMismatch(
                        eventID: event.eventID
                    )
                }
            }
            try session.stageMacroReady(preparedAssets)
            phase = .ready
            return next
        } catch {
            try rollbackPrecommit(original: error)
            throw error
        }
    }

    /// Registers and validates the exact result returned by the session's direct commit call.
    public func registerCommit(
        _ result: FightboxMacroCommitResult
    ) throws -> FightboxMacroCommitResult {
        guard phase == .ready, let preparation else {
            throw FightboxMacroLifecycleError.noReadyToken
        }
        guard result.status == 3 else {
            // The Ready transaction and exact provider generations remain retryable.
            throw FightboxMacroLifecycleError.commitNotTerminallyCommitted(
                status: result.status,
                ffiResult: result.ffiResultRawValue
            )
        }
        guard result.tokenID == preparation.tokenID else {
            throw FightboxMacroLifecycleError.commitIdentityMismatch("token")
        }
        guard result.eventCount == preparation.events.count else {
            throw FightboxMacroLifecycleError.commitIdentityMismatch("event count")
        }
        guard result.directGeneration != 0 else {
            throw FightboxMacroLifecycleError.commitIdentityMismatch("zero direct generation")
        }
        guard preparation.events.allSatisfy({ $0.activationFrame == result.effectiveFrame }) else {
            throw FightboxMacroLifecycleError.commitIdentityMismatch("effective frame")
        }
        let preparedMaximumTail = preparation.events
            .map(\.tailDeadlineFrame)
            .max() ?? result.effectiveFrame
        guard result.tailDeadlineFrame >= preparedMaximumTail,
              result.tailDeadlineFrame >= result.effectiveFrame
        else {
            throw FightboxMacroLifecycleError.commitIdentityMismatch("tail deadline")
        }
        commit = result
        remainingEventIDs = Set(preparation.events.map(\.eventID))
        phase = .committed
        return result
    }

    /// Rejects new work and rolls back only a pre-commit Ready token.
    public func beginShutdown() throws {
        switch phase {
        case .running:
            shutdownRequested = true
            phase = .draining
        case .ready:
            shutdownRequested = true
            phase = .quiescing
            try rollbackPrecommit(original: nil)
            phase = .draining
        case .committed:
            shutdownRequested = true
            phase = .draining
        case .draining:
            break
        case .preparing:
            // `prepare` is suspended only at provider awaits. Its next
            // identity check observes quiescing and executes exact rollback.
            shutdownRequested = true
            phase = .quiescing
        case .quiescing, .stopped:
            throw FightboxMacroLifecycleError.notRunning(phase)
        case .failedClosed:
            throw FightboxMacroLifecycleError.failedClosed(failureDetail ?? "unknown failure")
        }
    }

    /// Drains all ACKs currently available without ever polling past a consumed ACK.
    @discardableResult
    public func drainAvailableAcknowledgements() throws -> Int {
        if phase == .quiescing { return 0 }
        guard phase == .committed || phase == .draining else {
            throw FightboxMacroLifecycleError.notRunning(phase)
        }
        var finalized = 0
        while true {
            if consumedAcknowledgement == nil {
                guard let acknowledgement = try session.pollMacroAcknowledgement() else { break }
                // Polling consumes the Rust mailbox entry. Retain the exact ACK
                // before validation so hostile/stale identity can never be
                // skipped by a later poll.
                consumedAcknowledgement = Consumed(
                    acknowledgement: acknowledgement,
                    stage: .invalidIdentity
                )
                do {
                    try validate(acknowledgement)
                    consumedAcknowledgement?.stage = .awaitingProviderRelease
                } catch {
                    failureDetail = "consumed ACK validation failed: \(error)"
                    phase = .failedClosed
                    shutdownRequested = true
                    throw error
                }
            }
            guard var consumed = consumedAcknowledgement else { break }
            if consumed.stage == .invalidIdentity {
                throw FightboxMacroLifecycleError.failedClosed(
                    failureDetail ?? "invalid consumed acknowledgement"
                )
            }
            if consumed.stage == .awaitingProviderRelease {
                guard provider.releaseMacroAsset(consumed.acknowledgement.readiness) else {
                    // Retain the exact consumed ACK. The caller may explicitly retry.
                    throw FightboxMacroLifecycleError.providerReleaseRejected(
                        eventID: consumed.acknowledgement.eventID
                    )
                }
                consumed.stage = .awaitingRustFinalize
                consumedAcknowledgement = consumed
            }
            do {
                try session.finalizeMacroAcknowledgement(consumed.acknowledgement)
            } catch {
                // Provider was released exactly once; retry only Rust finalization.
                throw error
            }
            remainingEventIDs.remove(consumed.acknowledgement.eventID)
            consumedAcknowledgement = nil
            finalized += 1
            if remainingEventIDs.isEmpty {
                preparation = nil
                preparedAssets = []
                commit = nil
                if phase == .committed { phase = .running }
            }
        }
        return finalized
    }

    public func drainIsComplete() -> Bool {
        phase == .draining
            && preparation == nil
            && preparedAssets.isEmpty
            && commit == nil
            && remainingEventIDs.isEmpty
            && consumedAcknowledgement == nil
    }

    public func markStopped() throws {
        guard drainIsComplete() else { throw FightboxMacroLifecycleError.drainNotComplete }
        phase = .stopped
    }

    public func failClosed(_ detail: String) {
        failureDetail = detail
        shutdownRequested = true
        phase = .failedClosed
    }

    private func rollbackPrecommit(original: Error?) throws {
        guard let preparation else { return }
        let preserveFailedClosed = phase == .failedClosed
        var providerFailures: [String] = []
        for asset in preparedAssets.reversed() where !provider.releaseMacroAsset(asset.readiness) {
            providerFailures.append("provider release event \(asset.eventID)")
        }
        var failures = providerFailures
        // If an exact provider generation remains live, retain the Rust token
        // that names its ownership. Discarding would make cleanup impossible.
        if providerFailures.isEmpty {
            do {
                try session.discardMacroToken(preparation.tokenID)
            } catch {
                failures.append("Rust discard: \(error)")
            }
        }
        if failures.isEmpty {
            self.preparation = nil
            preparedAssets = []
            commit = nil
            remainingEventIDs = []
            consumedAcknowledgement = nil
            phase = preserveFailedClosed
                ? .failedClosed
                : (shutdownRequested ? .draining : .running)
            return
        }
        let prefix = original.map { "original=\($0); " } ?? ""
        let detail = prefix + failures.joined(separator: "; ")
        failureDetail = detail
        shutdownRequested = true
        phase = .failedClosed
        throw FightboxMacroLifecycleError.rollbackFailed(detail)
    }

    private func validate(_ acknowledgement: FightboxMacroAudioAcknowledgement) throws {
        guard let preparation, let commit else {
            throw FightboxMacroLifecycleError.acknowledgementIdentityMismatch("no commit")
        }
        guard acknowledgement.status <= 1 else {
            throw FightboxMacroLifecycleError.acknowledgementIdentityMismatch("status")
        }
        guard acknowledgement.tokenID == commit.tokenID,
              acknowledgement.directGeneration == commit.directGeneration,
              acknowledgement.effectiveFrame == commit.effectiveFrame,
              remainingEventIDs.contains(acknowledgement.eventID),
              let event = preparation.events.first(where: {
                  $0.eventID == acknowledgement.eventID
              }),
              let asset = preparedAssets.first(where: {
                  $0.eventID == acknowledgement.eventID
              }),
              acknowledgement.role == event.role,
              acknowledgement.readiness == asset.readiness
        else {
            throw FightboxMacroLifecycleError.acknowledgementIdentityMismatch("exact identity")
        }
    }
}
