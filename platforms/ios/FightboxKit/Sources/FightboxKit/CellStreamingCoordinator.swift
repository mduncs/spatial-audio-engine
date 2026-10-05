import Darwin
#if canImport(FightboxC)
import FightboxC
#endif
import Foundation
#if canImport(UIKit)
import UIKit
#endif

public struct FightboxCellIdentity: Sendable, Hashable, Codable {
    public var city: String
    public var cell: String

    public init(city: String, cell: String) {
        self.city = city
        self.cell = cell
    }
}

public struct FightboxCellPrepareEstimate: Sendable, Equatable, Codable {
    public var rawCellBytes: UInt64
    public var preparedResidentBytes: UInt64
    public var preparationScratchBytes: UInt64

    public init(
        rawCellBytes: UInt64,
        preparedResidentBytes: UInt64,
        preparationScratchBytes: UInt64
    ) {
        self.rawCellBytes = rawCellBytes
        self.preparedResidentBytes = preparedResidentBytes
        self.preparationScratchBytes = preparationScratchBytes
    }
}

public struct FightboxCellStreamingLimits: Sendable, Equatable, Codable {
    public var minimumAdvisoryReserveBytes: UInt64
    public var activePreparedTargetBytes: UInt64
    public var preparationPeakLimitBytes: UInt64
    public var rawCellHardLimitBytes: UInt64
    public var memoryWarningCooldownSeconds: TimeInterval

    public init(
        minimumAdvisoryReserveBytes: UInt64 = 512 * 1_048_576,
        activePreparedTargetBytes: UInt64 = 512 * 1_048_576,
        preparationPeakLimitBytes: UInt64 = 640 * 1_048_576,
        rawCellHardLimitBytes: UInt64 = 64 * 1_048_576,
        memoryWarningCooldownSeconds: TimeInterval = 60
    ) {
        self.minimumAdvisoryReserveBytes = minimumAdvisoryReserveBytes
        self.activePreparedTargetBytes = activePreparedTargetBytes
        self.preparationPeakLimitBytes = preparationPeakLimitBytes
        self.rawCellHardLimitBytes = rawCellHardLimitBytes
        self.memoryWarningCooldownSeconds = memoryWarningCooldownSeconds
    }
}

public enum FightboxThermalPressure: String, Sendable, Codable {
    case nominal
    case fair
    case serious
    case critical
    case unknown

    fileprivate init(_ state: ProcessInfo.ThermalState) {
        switch state {
        case .nominal: self = .nominal
        case .fair: self = .fair
        case .serious: self = .serious
        case .critical: self = .critical
        @unknown default: self = .unknown
        }
    }

    fileprivate var permitsPreparation: Bool {
        self == .nominal || self == .fair
    }
}

public protocol FightboxThermalSampling: Sendable {
    func sample() -> FightboxThermalPressure
}

public struct FightboxProcessThermalSampler: FightboxThermalSampling {
    public init() {}

    public func sample() -> FightboxThermalPressure {
        FightboxThermalPressure(ProcessInfo.processInfo.thermalState)
    }
}

public struct FightboxCellMemorySample: Sendable, Equatable, Codable {
    public var advisoryReserveBytes: UInt64
    public var processResidentBytes: UInt64
    public var sampledUptimeSeconds: TimeInterval

    public init(
        advisoryReserveBytes: UInt64,
        processResidentBytes: UInt64,
        sampledUptimeSeconds: TimeInterval
    ) {
        self.advisoryReserveBytes = advisoryReserveBytes
        self.processResidentBytes = processResidentBytes
        self.sampledUptimeSeconds = sampledUptimeSeconds
    }
}

public protocol FightboxCellMemorySampling: Sendable {
    /// Returns a newly measured sample. Implementations must not cache it.
    func sample() throws -> FightboxCellMemorySample
}

public enum FightboxCellMemorySamplerError: Error, Sendable {
    case taskInfo(kern_return_t)
    case unsupportedPlatform
}

#if os(iOS)
/// iOS process-pressure sampler. Every invocation calls
/// `os_proc_available_memory()` and `task_info`; neither result is reused for a
/// later admission decision.
public struct IOSFightboxCellMemorySampler: FightboxCellMemorySampling {
    public init() {}

    public func sample() throws -> FightboxCellMemorySample {
        var vmInfo = task_vm_info_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<task_vm_info_data_t>.size /
                MemoryLayout<integer_t>.size
        )
        let status = withUnsafeMutablePointer(to: &vmInfo) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(
                    mach_task_self_,
                    task_flavor_t(TASK_VM_INFO),
                    $0,
                    &count
                )
            }
        }
        guard status == KERN_SUCCESS else {
            throw FightboxCellMemorySamplerError.taskInfo(status)
        }
        return FightboxCellMemorySample(
            advisoryReserveBytes: UInt64(os_proc_available_memory()),
            processResidentBytes: UInt64(vmInfo.phys_footprint),
            sampledUptimeSeconds: ProcessInfo.processInfo.systemUptime
        )
    }
}

public typealias FightboxPlatformCellMemorySampler = IOSFightboxCellMemorySampler
#else
/// Non-iOS builds expose the pure coordinator for deterministic host tests, but
/// have no valid approximation for iOS advisory memory. Production callers
/// must remain on the iOS sampler; host tests inject an explicit fake.
public struct FightboxPlatformCellMemorySampler: FightboxCellMemorySampling {
    public init() {}

    public func sample() throws -> FightboxCellMemorySample {
        throw FightboxCellMemorySamplerError.unsupportedPlatform
    }
}
#endif

public struct FightboxCellDescriptor: Sendable {
    public var identity: FightboxCellIdentity
    public var packageURL: URL
    public var bakeURL: URL
    public var programs: [FightboxSpatialSourceProgram]
    public var environmentalOrder: UInt32
    public var quality: FightboxSpatialQuality
    public var defaultSourceLevelDB: Float
    public var listenerPose: FightboxPose
    public var listenerLinearVelocityMPS: SIMD3<Float>
    public var sources: [FightboxSpatialSourceState]
    public var estimate: FightboxCellPrepareEstimate

    public init(
        identity: FightboxCellIdentity,
        packageURL: URL,
        bakeURL: URL,
        programs: [FightboxSpatialSourceProgram],
        environmentalOrder: UInt32 = 2,
        quality: FightboxSpatialQuality,
        defaultSourceLevelDB: Float = 0,
        listenerPose: FightboxPose,
        listenerLinearVelocityMPS: SIMD3<Float> = .zero,
        sources: [FightboxSpatialSourceState],
        estimate: FightboxCellPrepareEstimate
    ) {
        self.identity = identity
        self.packageURL = packageURL
        self.bakeURL = bakeURL
        self.programs = programs
        self.environmentalOrder = environmentalOrder
        self.quality = quality
        self.defaultSourceLevelDB = defaultSourceLevelDB
        self.listenerPose = listenerPose
        self.listenerLinearVelocityMPS = listenerLinearVelocityMPS
        self.sources = sources
        self.estimate = estimate
    }
}

public protocol FightboxPreparedCellPayload: AnyObject, Sendable {
    var identity: FightboxCellIdentity { get }
    var residentBytes: UInt64 { get }
}

public protocol FightboxTailRetirementToken: AnyObject, Sendable {
    var identity: FightboxCellIdentity { get }
}

public protocol FightboxCellStreamingBackend: Sendable {
    var supportsPreparedAdoption: Bool { get }

    func prepareCell(
        _ descriptor: FightboxCellDescriptor
    ) async throws -> any FightboxPreparedCellPayload

    /// These lifecycle operations are deliberately non-suspending. They remain
    /// on the control plane, never the audio callback; keeping them on the actor
    /// executor prevents pressure and route tasks from re-entering the state
    /// machine between ownership mutation and telemetry publication.
    func cancelPreparation(for identity: FightboxCellIdentity)

    func releasePrepared(_ payload: any FightboxPreparedCellPayload)

    /// Atomically gives the prepared cell new-event authority and returns the
    /// former active world's tail-retirement token.
    func adoptPrepared(
        _ payload: any FightboxPreparedCellPayload
    ) throws -> any FightboxTailRetirementToken

    func isTailRetirementComplete(
        _ token: any FightboxTailRetirementToken
    ) throws -> Bool

    func finishTailRetirement(
        _ token: any FightboxTailRetirementToken
    )
}

public enum FightboxCellBackendError: Error, Sendable, CustomStringConvertible {
    case invalidPreparedPayload
    case incompatibleActiveSession
    case incompatibleCellRoute

    public var description: String {
        switch self {
        case .invalidPreparedPayload:
            return "Prepared cell payload is not a Fightbox neutral cell"
        case .incompatibleActiveSession:
            return "Prepared cell belongs to a different active session"
        case .incompatibleCellRoute:
            return "Cell source layout or environmental order differs from the active route"
        }
    }
}

public final class FightboxPreparedNeutralCell: FightboxPreparedCellPayload,
    @unchecked Sendable
{
    public let identity: FightboxCellIdentity
    public let residentBytes: UInt64
    public let session: FightboxNeutralSpatialSession
    private var preparedHandle: OpaquePointer?

    public init(
        identity: FightboxCellIdentity,
        residentBytes: UInt64,
        session: FightboxNeutralSpatialSession
    ) {
        self.identity = identity
        self.residentBytes = residentBytes
        self.session = session
        preparedHandle = nil
    }

    fileprivate init(
        identity: FightboxCellIdentity,
        residentBytes: UInt64,
        session: FightboxNeutralSpatialSession,
        preparedHandle: OpaquePointer
    ) {
        self.identity = identity
        self.residentBytes = residentBytes
        self.session = session
        self.preparedHandle = preparedHandle
    }

    deinit {
        if let preparedHandle {
            session.destroyPreparedCellHandle(preparedHandle)
        }
    }

    fileprivate func offer() throws {
        guard let preparedHandle else {
            throw FightboxCellBackendError.invalidPreparedPayload
        }
        try session.offerPreparedCellHandle(preparedHandle)
        self.preparedHandle = nil
    }

    fileprivate func releasePrepared() {
        guard let preparedHandle else { return }
        session.destroyPreparedCellHandle(preparedHandle)
        self.preparedHandle = nil
    }
}

private final class FightboxNeutralTailToken: FightboxTailRetirementToken,
    @unchecked Sendable
{
    let identity: FightboxCellIdentity
    let session: FightboxNeutralSpatialSession

    init(
        identity: FightboxCellIdentity,
        session: FightboxNeutralSpatialSession
    ) {
        self.identity = identity
        self.session = session
    }
}

/// Concrete bridge from the Swift pressure coordinator into the live neutral
/// Steam graph. Neighbor construction happens off-thread; adoption publishes
/// the prepared graph to the callback's bounded swap channel.
public final class FightboxNeutralSessionCellBackend: FightboxCellStreamingBackend,
    @unchecked Sendable
{
    private let activeSession: FightboxNeutralSpatialSession
    private let memorySampler: any FightboxCellMemorySampling
    private let identityLock = NSLock()
    private var activeIdentity: FightboxCellIdentity

    public init(
        active: FightboxPreparedNeutralCell,
        memorySampler: any FightboxCellMemorySampling = FightboxPlatformCellMemorySampler()
    ) {
        activeSession = active.session
        activeIdentity = active.identity
        self.memorySampler = memorySampler
    }

    public var supportsPreparedAdoption: Bool { true }

    public func prepareCell(
        _ descriptor: FightboxCellDescriptor
    ) async throws -> any FightboxPreparedCellPayload {
        let sampler = memorySampler
        let session = activeSession
        return try await Task.detached(priority: .userInitiated) {
            try Task.checkCancellation()
            guard descriptor.programs.count == session.programs.count,
                  descriptor.environmentalOrder == session.environmentalOrder
            else {
                throw FightboxCellBackendError.incompatibleCellRoute
            }
            let before = try sampler.sample()
            try session.updateControlFrame(
                listenerPose: descriptor.listenerPose,
                listenerLinearVelocityMPS: descriptor.listenerLinearVelocityMPS,
                sources: descriptor.sources
            )
            try Task.checkCancellation()
            let handle = try session.prepareCellHandle(
                packageURL: descriptor.packageURL,
                bakeURL: descriptor.bakeURL,
                cityKey: Self.stableKey(descriptor.identity.city),
                cellKey: Self.stableKey(descriptor.identity.cell),
                rawCellBytes: descriptor.estimate.rawCellBytes,
                preparedResidentBytes: descriptor.estimate.preparedResidentBytes
            )
            do {
                try Task.checkCancellation()
                let after = try sampler.sample()
                let measuredIncrement = after.processResidentBytes > before.processResidentBytes
                    ? after.processResidentBytes - before.processResidentBytes
                    : 0
                return FightboxPreparedNeutralCell(
                    identity: descriptor.identity,
                    residentBytes: max(
                        descriptor.estimate.preparedResidentBytes,
                        measuredIncrement
                    ),
                    session: session,
                    preparedHandle: handle
                )
            } catch {
                session.destroyPreparedCellHandle(handle)
                throw error
            }
        }.value
    }

    public func cancelPreparation(for _: FightboxCellIdentity) {
        // The coordinator cancels the owning Task. The concrete loader checks
        // cancellation between construction, control publication, and prepare.
    }

    public func releasePrepared(_ payload: any FightboxPreparedCellPayload) {
        (payload as? FightboxPreparedNeutralCell)?.releasePrepared()
    }

    public func adoptPrepared(
        _ payload: any FightboxPreparedCellPayload
    ) throws -> any FightboxTailRetirementToken {
        guard let payload = payload as? FightboxPreparedNeutralCell else {
            throw FightboxCellBackendError.invalidPreparedPayload
        }
        guard payload.session === activeSession else {
            throw FightboxCellBackendError.incompatibleActiveSession
        }
        try payload.offer()
        let retiringIdentity = identityLock.withLock {
            let identity = activeIdentity
            activeIdentity = payload.identity
            return identity
        }
        return FightboxNeutralTailToken(
            identity: retiringIdentity,
            session: activeSession
        )
    }

    public func isTailRetirementComplete(
        _ token: any FightboxTailRetirementToken
    ) throws -> Bool {
        guard let token = token as? FightboxNeutralTailToken,
              token.session === activeSession
        else {
            throw FightboxCellBackendError.incompatibleActiveSession
        }
        let phase = try activeSession.cellStreamPhase()
        return phase == UInt32(FB_CELL_STREAM_PHASE_TAIL_COMPLETE_V2)
            || phase == UInt32(FB_CELL_STREAM_PHASE_IDLE_V2)
    }

    public func finishTailRetirement(
        _ token: any FightboxTailRetirementToken
    ) {
        guard let token = token as? FightboxNeutralTailToken,
              token.session === activeSession
        else { return }
        try? activeSession.collectRetiredCell()
    }

    private static func stableKey(_ value: String) -> (UInt64, UInt64) {
        var high: UInt64 = 0xcbf29ce484222325
        var low: UInt64 = 0x84222325cbf29ce4
        for byte in value.utf8 {
            high = (high ^ UInt64(byte)) &* 0x100000001b3
            low = (low ^ UInt64(byte &+ 0x5b)) &* 0x100000001b3
        }
        if high == 0 && low == 0 { low = 1 }
        return (high, low)
    }
}

public enum FightboxCellPrepareRefusal: Sendable, Equatable, Codable {
    case activeIdentityChanged(
        expected: FightboxCellIdentity,
        delivered: FightboxCellIdentity
    )
    case alreadyActive
    case tailRetiring
    case rawCellTooLarge(requested: UInt64, limit: UInt64)
    case advisoryReserveTooLow(sampled: UInt64, required: UInt64)
    case activePreparedTargetExceeded(projected: UInt64, limit: UInt64)
    case preparationPeakExceeded(projected: UInt64, limit: UInt64)
    case memoryWarningCooldown(remainingSeconds: TimeInterval)
    case thermalPressure(FightboxThermalPressure)
    case memorySampleFailed(String)
}

public enum FightboxCellNeighborPhase: String, Sendable, Codable {
    case queued
    case preparing
    case cancellingStaleDirection
    case prepared
}

public struct FightboxCellNeighborTelemetry: Sendable, Codable {
    public var identity: FightboxCellIdentity
    public var phase: FightboxCellNeighborPhase
    public var estimatedResidentBytes: UInt64
    public var replacement: FightboxCellIdentity?
}

public struct FightboxCellStreamingTelemetry: Sendable, Codable {
    public var active: FightboxCellIdentity
    public var neighbor: FightboxCellNeighborTelemetry?
    public var tailRetiring: FightboxCellIdentity?
    public var residentWorldBytes: UInt64
    public var lastMemorySample: FightboxCellMemorySample?
    public var thermalPressure: FightboxThermalPressure
    public var memoryWarningCount: UInt64
    public var lastPrepareDurationSeconds: TimeInterval?
    public var lastRefusal: FightboxCellPrepareRefusal?
    public var lastCancellation: String?
    public var lastFailure: String?
    public var coarseMacroFallback: Bool
    public var linkageReadyForAdoption: Bool
}

public enum FightboxCellPrepareRequestResult: Sendable, Equatable {
    case queued(UInt64)
    case alreadyPending(UInt64)
    case replacementPending(UInt64)
    case refused(FightboxCellPrepareRefusal)
}

public enum FightboxCellStreamingCoordinatorError: Error, Sendable {
    case noPreparedNeighbor
    case preparationNotReady
    case tailAlreadyRetiring
}

private struct FightboxQueuedCell: Sendable {
    var ticket: UInt64
    var descriptor: FightboxCellDescriptor
}

private enum FightboxNeighborState: Sendable {
    case queued(FightboxQueuedCell)
    case preparing(
        queued: FightboxQueuedCell,
        startedUptime: TimeInterval,
        task: Task<Result<any FightboxPreparedCellPayload, Error>, Never>
    )
    case cancelling(
        queued: FightboxQueuedCell,
        replacement: FightboxCellDescriptor?,
        task: Task<Result<any FightboxPreparedCellPayload, Error>, Never>
    )
    case prepared(
        queued: FightboxQueuedCell,
        payload: any FightboxPreparedCellPayload,
        duration: TimeInterval
    )
}

private struct FightboxRetiringCell: Sendable {
    var identity: FightboxCellIdentity
    var residentBytes: UInt64
    var payload: any FightboxPreparedCellPayload
    var token: any FightboxTailRetirementToken
}

/// Async control-plane coordinator. It never runs I/O, allocation, or world
/// transitions from the audio callback.
public actor FightboxCellStreamingCoordinator {
    private let backend: any FightboxCellStreamingBackend
    private let memorySampler: any FightboxCellMemorySampling
    private let thermalSampler: any FightboxThermalSampling
    private let limits: FightboxCellStreamingLimits

    private var active: any FightboxPreparedCellPayload
    private var neighbor: FightboxNeighborState?
    private var retiring: FightboxRetiringCell?
    private var nextTicket: UInt64 = 1
    private var lastMemorySample: FightboxCellMemorySample?
    private var lastMemoryWarningUptime: TimeInterval?
    private var memoryWarningCount: UInt64 = 0
    private var lastPrepareDuration: TimeInterval?
    private var lastRefusal: FightboxCellPrepareRefusal?
    private var lastCancellation: String?
    private var lastFailure: String?
    private var coarseMacroFallback = false

    public init(
        active: any FightboxPreparedCellPayload,
        backend: any FightboxCellStreamingBackend,
        memorySampler: any FightboxCellMemorySampling = FightboxPlatformCellMemorySampler(),
        thermalSampler: any FightboxThermalSampling = FightboxProcessThermalSampler(),
        limits: FightboxCellStreamingLimits = FightboxCellStreamingLimits()
    ) {
        self.active = active
        self.backend = backend
        self.memorySampler = memorySampler
        self.thermalSampler = thermalSampler
        self.limits = limits
    }

    @discardableResult
    public func requestNeighbor(
        _ descriptor: FightboxCellDescriptor,
        expectedActiveIdentity: FightboxCellIdentity? = nil
    ) async -> FightboxCellPrepareRequestResult {
        if let expectedActiveIdentity, expectedActiveIdentity != active.identity {
            return refuse(.activeIdentityChanged(
                expected: expectedActiveIdentity,
                delivered: active.identity
            ))
        }
        if descriptor.identity == active.identity {
            return refuse(.alreadyActive)
        }
        if retiring != nil {
            return refuse(.tailRetiring)
        }

        if let neighbor {
            switch neighbor {
            case let .queued(queued):
                if queued.descriptor.identity == descriptor.identity {
                    return .alreadyPending(queued.ticket)
                }
                guard admissionRefusal(for: descriptor) == nil else {
                    return .refused(lastRefusal!)
                }
                let replacement = makeQueued(descriptor)
                self.neighbor = .queued(replacement)
                lastCancellation = "replaced queued \(queued.descriptor.identity.cell) with \(descriptor.identity.cell)"
                scheduleQueuedStart(ticket: replacement.ticket)
                return .replacementPending(replacement.ticket)

            case let .preparing(queued, _, task):
                if queued.descriptor.identity == descriptor.identity {
                    return .alreadyPending(queued.ticket)
                }
                task.cancel()
                self.neighbor = .cancelling(
                    queued: queued,
                    replacement: descriptor,
                    task: task
                )
                lastCancellation = "cancelling stale direction \(queued.descriptor.identity.cell) for \(descriptor.identity.cell)"
                coarseMacroFallback = true
                backend.cancelPreparation(for: queued.descriptor.identity)
                return .replacementPending(queued.ticket)

            case let .cancelling(queued, _, task):
                self.neighbor = .cancelling(
                    queued: queued,
                    replacement: descriptor,
                    task: task
                )
                lastCancellation = "updated pending replacement to \(descriptor.identity.cell)"
                return .replacementPending(queued.ticket)

            case let .prepared(queued, payload, _):
                if queued.descriptor.identity == descriptor.identity {
                    return .alreadyPending(queued.ticket)
                }
                backend.releasePrepared(payload)
                self.neighbor = nil
                lastCancellation = "released stale prepared \(queued.descriptor.identity.cell) for \(descriptor.identity.cell)"
            }
        }

        if let reason = admissionRefusal(for: descriptor) {
            return refuse(reason)
        }
        let queued = makeQueued(descriptor)
        neighbor = .queued(queued)
        lastRefusal = nil
        coarseMacroFallback = false
        scheduleQueuedStart(ticket: queued.ticket)
        return .queued(queued.ticket)
    }

    public func activatePreparedNeighbor() async throws {
        guard retiring == nil else {
            throw FightboxCellStreamingCoordinatorError.tailAlreadyRetiring
        }
        guard let neighbor else {
            throw FightboxCellStreamingCoordinatorError.noPreparedNeighbor
        }
        guard case let .prepared(queued, payload, duration) = neighbor else {
            throw FightboxCellStreamingCoordinatorError.preparationNotReady
        }

        do {
            let tail = try backend.adoptPrepared(payload)
            let old = active
            active = payload
            self.neighbor = nil
            retiring = FightboxRetiringCell(
                identity: old.identity,
                residentBytes: old.residentBytes,
                payload: old,
                token: tail
            )
            lastPrepareDuration = duration
            coarseMacroFallback = false
            lastFailure = nil
            _ = queued
        } catch {
            lastFailure = String(describing: error)
            coarseMacroFallback = true
            throw error
        }
    }

    /// Called from a control timer or explicit backend notification, never the
    /// audio callback. A third world remains refused until this succeeds.
    @discardableResult
    public func refreshTailRetirement() async throws -> Bool {
        guard let retiring else { return true }
        guard try backend.isTailRetirementComplete(retiring.token) else {
            return false
        }
        backend.finishTailRetirement(retiring.token)
        self.retiring = nil
        return true
    }

    public func recordMemoryWarning() async {
        memoryWarningCount &+= 1
        lastMemoryWarningUptime = ProcessInfo.processInfo.systemUptime
        lastRefusal = .memoryWarningCooldown(
            remainingSeconds: limits.memoryWarningCooldownSeconds
        )
        coarseMacroFallback = true
        await cancelOrReleaseNeighbor(reason: "iOS memory warning")
    }

    public func respondToCurrentThermalPressure() async {
        let thermal = thermalSampler.sample()
        guard !thermal.permitsPreparation else { return }
        lastRefusal = .thermalPressure(thermal)
        coarseMacroFallback = true
        await cancelOrReleaseNeighbor(reason: "thermal pressure \(thermal.rawValue)")
    }

    public func telemetry() -> FightboxCellStreamingTelemetry {
        let neighborTelemetry: FightboxCellNeighborTelemetry?
        let neighborResidentBytes: UInt64
        switch neighbor {
        case let .queued(queued):
            neighborTelemetry = makeNeighborTelemetry(queued, phase: .queued)
            neighborResidentBytes = 0
        case let .preparing(queued, _, _):
            neighborTelemetry = makeNeighborTelemetry(queued, phase: .preparing)
            neighborResidentBytes = 0
        case let .cancelling(queued, replacement, _):
            var value = makeNeighborTelemetry(
                queued,
                phase: .cancellingStaleDirection
            )
            value.replacement = replacement?.identity
            neighborTelemetry = value
            neighborResidentBytes = 0
        case let .prepared(queued, payload, _):
            neighborTelemetry = makeNeighborTelemetry(queued, phase: .prepared)
            neighborResidentBytes = payload.residentBytes
        case nil:
            neighborTelemetry = nil
            neighborResidentBytes = 0
        }
        return FightboxCellStreamingTelemetry(
            active: active.identity,
            neighbor: neighborTelemetry,
            tailRetiring: retiring?.identity,
            residentWorldBytes: active.residentBytes +
                neighborResidentBytes + (retiring?.residentBytes ?? 0),
            lastMemorySample: lastMemorySample,
            thermalPressure: thermalSampler.sample(),
            memoryWarningCount: memoryWarningCount,
            lastPrepareDurationSeconds: lastPrepareDuration,
            lastRefusal: lastRefusal,
            lastCancellation: lastCancellation,
            lastFailure: lastFailure,
            coarseMacroFallback: coarseMacroFallback,
            linkageReadyForAdoption: backend.supportsPreparedAdoption
        )
    }

    public func telemetryJSON() throws -> String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(telemetry())
        guard let text = String(data: data, encoding: .utf8) else {
            throw FightboxError.invalidTelemetry
        }
        return text
    }

    private func scheduleQueuedStart(ticket: UInt64) {
        Task { [weak self] in
            await Task.yield()
            await self?.startQueued(ticket: ticket)
        }
    }

    private func startQueued(ticket: UInt64) {
        guard case let .queued(queued)? = neighbor, queued.ticket == ticket else {
            return
        }
        if let reason = admissionRefusal(for: queued.descriptor) {
            neighbor = nil
            _ = refuse(reason)
            return
        }

        let started = ProcessInfo.processInfo.systemUptime
        let backend = self.backend
        let descriptor = queued.descriptor
        let task = Task<Result<any FightboxPreparedCellPayload, Error>, Never> {
            do {
                return .success(try await backend.prepareCell(descriptor))
            } catch {
                return .failure(error)
            }
        }
        neighbor = .preparing(
            queued: queued,
            startedUptime: started,
            task: task
        )
        Task { [weak self] in
            let result = await task.value
            await self?.preparationFinished(
                ticket: ticket,
                startedUptime: started,
                result: result
            )
        }
    }

    private func preparationFinished(
        ticket: UInt64,
        startedUptime: TimeInterval,
        result: Result<any FightboxPreparedCellPayload, Error>
    ) async {
        guard let neighbor else {
            if case let .success(payload) = result {
                backend.releasePrepared(payload)
            }
            return
        }

        if case let .cancelling(queued, replacement, _) = neighbor,
           queued.ticket == ticket
        {
            if case let .success(payload) = result {
                backend.releasePrepared(payload)
            }
            self.neighbor = nil
            if let replacement {
                _ = await requestNeighbor(replacement)
            }
            return
        }

        guard case let .preparing(queued, _, _) = neighbor,
              queued.ticket == ticket
        else {
            if case let .success(payload) = result {
                backend.releasePrepared(payload)
            }
            return
        }

        switch result {
        case let .failure(error):
            self.neighbor = nil
            lastFailure = String(describing: error)
            coarseMacroFallback = true

        case let .success(payload):
            guard payload.identity == queued.descriptor.identity else {
                self.neighbor = nil
                lastFailure = "prepared payload identity mismatch"
                coarseMacroFallback = true
                backend.releasePrepared(payload)
                return
            }
            if let reason = completionRefusal(for: payload) {
                self.neighbor = nil
                _ = refuse(reason)
                backend.releasePrepared(payload)
                return
            }
            let duration = max(
                0,
                ProcessInfo.processInfo.systemUptime - startedUptime
            )
            self.neighbor = .prepared(
                queued: queued,
                payload: payload,
                duration: duration
            )
            lastPrepareDuration = duration
            lastFailure = nil
            coarseMacroFallback = false
        }
    }

    private func admissionRefusal(
        for descriptor: FightboxCellDescriptor
    ) -> FightboxCellPrepareRefusal? {
        let estimate = descriptor.estimate
        if estimate.rawCellBytes > limits.rawCellHardLimitBytes {
            return recordRefusal(.rawCellTooLarge(
                requested: estimate.rawCellBytes,
                limit: limits.rawCellHardLimitBytes
            ))
        }
        let thermal = thermalSampler.sample()
        guard thermal.permitsPreparation else {
            return recordRefusal(.thermalPressure(thermal))
        }
        if let lastMemoryWarningUptime {
            let elapsed = ProcessInfo.processInfo.systemUptime - lastMemoryWarningUptime
            if elapsed < limits.memoryWarningCooldownSeconds {
                return recordRefusal(.memoryWarningCooldown(
                    remainingSeconds: limits.memoryWarningCooldownSeconds - elapsed
                ))
            }
        }

        let sample: FightboxCellMemorySample
        do {
            sample = try memorySampler.sample()
            lastMemorySample = sample
        } catch {
            return recordRefusal(.memorySampleFailed(String(describing: error)))
        }
        if sample.advisoryReserveBytes < limits.minimumAdvisoryReserveBytes {
            return recordRefusal(.advisoryReserveTooLow(
                sampled: sample.advisoryReserveBytes,
                required: limits.minimumAdvisoryReserveBytes
            ))
        }
        let activePrepared = sample.processResidentBytes +
            estimate.preparedResidentBytes
        if activePrepared > limits.activePreparedTargetBytes {
            return recordRefusal(.activePreparedTargetExceeded(
                projected: activePrepared,
                limit: limits.activePreparedTargetBytes
            ))
        }
        let peak = activePrepared + estimate.preparationScratchBytes
        if peak > limits.preparationPeakLimitBytes {
            return recordRefusal(.preparationPeakExceeded(
                projected: peak,
                limit: limits.preparationPeakLimitBytes
            ))
        }
        return nil
    }

    private func completionRefusal(
        for payload: any FightboxPreparedCellPayload
    ) -> FightboxCellPrepareRefusal? {
        let thermal = thermalSampler.sample()
        guard thermal.permitsPreparation else {
            return recordRefusal(.thermalPressure(thermal))
        }
        do {
            let sample = try memorySampler.sample()
            lastMemorySample = sample
            if sample.advisoryReserveBytes < limits.minimumAdvisoryReserveBytes {
                return recordRefusal(.advisoryReserveTooLow(
                    sampled: sample.advisoryReserveBytes,
                    required: limits.minimumAdvisoryReserveBytes
                ))
            }
            if sample.processResidentBytes > limits.activePreparedTargetBytes {
                return recordRefusal(.activePreparedTargetExceeded(
                    projected: sample.processResidentBytes,
                    limit: limits.activePreparedTargetBytes
                ))
            }
            if payload.residentBytes > limits.activePreparedTargetBytes {
                return recordRefusal(.activePreparedTargetExceeded(
                    projected: payload.residentBytes,
                    limit: limits.activePreparedTargetBytes
                ))
            }
        } catch {
            return recordRefusal(.memorySampleFailed(String(describing: error)))
        }
        return nil
    }

    private func cancelOrReleaseNeighbor(reason: String) async {
        guard let neighbor else { return }
        lastCancellation = reason
        switch neighbor {
        case .queued:
            self.neighbor = nil
        case let .preparing(queued, _, task):
            task.cancel()
            self.neighbor = .cancelling(
                queued: queued,
                replacement: nil,
                task: task
            )
            backend.cancelPreparation(for: queued.descriptor.identity)
        case let .cancelling(queued, _, task):
            self.neighbor = .cancelling(
                queued: queued,
                replacement: nil,
                task: task
            )
        case let .prepared(_, payload, _):
            self.neighbor = nil
            backend.releasePrepared(payload)
        }
    }

    private func makeQueued(_ descriptor: FightboxCellDescriptor) -> FightboxQueuedCell {
        let ticket = nextTicket
        nextTicket = nextTicket &+ 1
        if nextTicket == 0 { nextTicket = 1 }
        return FightboxQueuedCell(ticket: ticket, descriptor: descriptor)
    }

    private func makeNeighborTelemetry(
        _ queued: FightboxQueuedCell,
        phase: FightboxCellNeighborPhase
    ) -> FightboxCellNeighborTelemetry {
        FightboxCellNeighborTelemetry(
            identity: queued.descriptor.identity,
            phase: phase,
            estimatedResidentBytes: queued.descriptor.estimate.preparedResidentBytes,
            replacement: nil
        )
    }

    private func recordRefusal(
        _ reason: FightboxCellPrepareRefusal
    ) -> FightboxCellPrepareRefusal {
        lastRefusal = reason
        coarseMacroFallback = true
        return reason
    }

    private func refuse(
        _ reason: FightboxCellPrepareRefusal
    ) -> FightboxCellPrepareRequestResult {
        .refused(recordRefusal(reason))
    }
}

#if canImport(UIKit)
/// Main-thread bridge for UIKit memory warnings and process thermal changes.
/// It never starts preparation itself; it only asks the coordinator to cancel
/// or shed the one optional neighbor.
private final class FightboxObserverTokens: @unchecked Sendable {
    var values: [NSObjectProtocol] = []

    deinit {
        for value in values {
            NotificationCenter.default.removeObserver(value)
        }
    }
}

@MainActor
public final class FightboxCellPressureObserver {
    private let coordinator: FightboxCellStreamingCoordinator
    private let observers = FightboxObserverTokens()

    public init(coordinator: FightboxCellStreamingCoordinator) {
        self.coordinator = coordinator
        let center = NotificationCenter.default
        observers.values.append(center.addObserver(
            forName: UIApplication.didReceiveMemoryWarningNotification,
            object: nil,
            queue: .main
        ) { [weak coordinator] _ in
            guard let coordinator else { return }
            Task { await coordinator.recordMemoryWarning() }
        })
        observers.values.append(center.addObserver(
            forName: ProcessInfo.thermalStateDidChangeNotification,
            object: nil,
            queue: .main
        ) { [weak coordinator] _ in
            guard let coordinator else { return }
            Task { await coordinator.respondToCurrentThermalPressure() }
        })
    }

}
#endif
