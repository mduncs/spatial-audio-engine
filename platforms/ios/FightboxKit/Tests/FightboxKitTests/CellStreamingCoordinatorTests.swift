import Foundation
import XCTest
@testable import FightboxKit

@available(macOS 10.15, *)
final class CellStreamingCoordinatorTests: XCTestCase {
    func testPrepareAdoptRefusesThirdWorldUntilTailCompletes() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(activeIdentity: active.identity)
        let coordinator = makeCoordinator(active: active, backend: backend)

        let queued = await coordinator.requestNeighbor(
            descriptor("b"),
            expectedActiveIdentity: active.identity
        )
        guard case .queued = queued else {
            return XCTFail("expected the first neighbor to queue, got \(queued)")
        }
        _ = try await waitForTelemetry(coordinator) {
            $0.neighbor?.identity == self.identity("b") &&
                $0.neighbor?.phase == .prepared
        }

        try await coordinator.activatePreparedNeighbor()
        var telemetry = await coordinator.telemetry()
        XCTAssertEqual(telemetry.active, identity("b"))
        XCTAssertEqual(telemetry.tailRetiring, identity("a"))
        XCTAssertNil(telemetry.neighbor)
        XCTAssertEqual(telemetry.residentWorldBytes, 200)

        let refused = await coordinator.requestNeighbor(
            descriptor("c"),
            expectedActiveIdentity: identity("b")
        )
        guard case .refused(.tailRetiring) = refused else {
            return XCTFail("expected third-world refusal, got \(refused)")
        }
        do {
            try await coordinator.activatePreparedNeighbor()
            XCTFail("activation must fail while an old tail is still resident")
        } catch FightboxCellStreamingCoordinatorError.tailAlreadyRetiring {
            // Expected: this is the Swift-side third-world gate.
        }
        let incompleteTail = try await coordinator.refreshTailRetirement()
        XCTAssertFalse(incompleteTail)

        backend.setTailComplete(true)
        let completedTail = try await coordinator.refreshTailRetirement()
        XCTAssertTrue(completedTail)
        telemetry = await coordinator.telemetry()
        XCTAssertEqual(telemetry.active, identity("b"))
        XCTAssertNil(telemetry.tailRetiring)
        XCTAssertEqual(telemetry.residentWorldBytes, 100)
        let finishedTailIdentities = backend.finishedTailIdentities()
        XCTAssertEqual(finishedTailIdentities, [identity("a")])

        let next = await coordinator.requestNeighbor(
            descriptor("c"),
            expectedActiveIdentity: identity("b")
        )
        guard case .queued = next else {
            return XCTFail("expected preparation after terminal tail, got \(next)")
        }
    }

    func testCancellingStalePreparationCannotStrandReplacement() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(
            activeIdentity: active.identity,
            delayedIdentities: [identity("b")]
        )
        let coordinator = makeCoordinator(active: active, backend: backend)

        _ = await coordinator.requestNeighbor(descriptor("b"))
        _ = try await waitForTelemetry(coordinator) {
            $0.neighbor?.identity == self.identity("b") &&
                $0.neighbor?.phase == .preparing
        }

        let replacement = await coordinator.requestNeighbor(descriptor("c"))
        guard case .replacementPending = replacement else {
            return XCTFail("expected replacement pending, got \(replacement)")
        }
        var telemetry = await coordinator.telemetry()
        XCTAssertEqual(telemetry.neighbor?.identity, identity("b"))
        XCTAssertEqual(telemetry.neighbor?.phase, .cancellingStaleDirection)
        XCTAssertEqual(telemetry.neighbor?.replacement, identity("c"))
        XCTAssertTrue(telemetry.coarseMacroFallback)

        await backend.completeDelayedPreparation(identity("b"))
        telemetry = try await waitForTelemetry(coordinator) {
            $0.neighbor?.identity == self.identity("c") &&
                $0.neighbor?.phase == .prepared
        }
        XCTAssertEqual(telemetry.active, identity("a"))
        XCTAssertTrue(telemetry.lastCancellation?.contains("stale direction") == true)
        let cancelledIdentities = backend.cancelledIdentities()
        XCTAssertEqual(cancelledIdentities, [identity("b")])
        let releasedIdentities = backend.releasedIdentities()
        XCTAssertEqual(releasedIdentities, [identity("b")])

        try await coordinator.activatePreparedNeighbor()
        let activeAfterReplacement = await coordinator.telemetry().active
        XCTAssertEqual(activeAfterReplacement, identity("c"))
    }

    func testLowAdvisoryReserveRefusesBeforePreparation() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(activeIdentity: active.identity)
        let sample = FightboxCellMemorySample(
            advisoryReserveBytes: 999,
            processResidentBytes: 100,
            sampledUptimeSeconds: 42
        )
        let coordinator = makeCoordinator(
            active: active,
            backend: backend,
            memorySample: sample
        )

        let result = await coordinator.requestNeighbor(descriptor("b"))
        guard case let .refused(.advisoryReserveTooLow(sampled, required)) = result else {
            return XCTFail("expected advisory-reserve refusal, got \(result)")
        }
        XCTAssertEqual(sampled, 999)
        XCTAssertEqual(required, 1_000)
        let telemetry = await coordinator.telemetry()
        XCTAssertEqual(telemetry.lastMemorySample, sample)
        XCTAssertNil(telemetry.neighbor)
        XCTAssertTrue(telemetry.coarseMacroFallback)
        XCTAssertEqual(backend.preparedIdentities(), [])
    }

    func testSeriousThermalPressureRefusesBeforePreparation() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(activeIdentity: active.identity)
        let coordinator = makeCoordinator(
            active: active,
            backend: backend,
            thermalPressure: .serious
        )

        let result = await coordinator.requestNeighbor(descriptor("b"))
        guard case .refused(.thermalPressure(.serious)) = result else {
            return XCTFail("expected serious thermal refusal, got \(result)")
        }
        let telemetry = await coordinator.telemetry()
        XCTAssertEqual(telemetry.thermalPressure, .serious)
        XCTAssertNil(telemetry.neighbor)
        XCTAssertTrue(telemetry.coarseMacroFallback)
        XCTAssertEqual(backend.preparedIdentities(), [])
    }

    func testMemoryWarningReleasesPreparedNeighborAndFailsClosed() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(activeIdentity: active.identity)
        let coordinator = makeCoordinator(active: active, backend: backend)

        let mismatch = await coordinator.requestNeighbor(
            descriptor("b"),
            expectedActiveIdentity: identity("wrong")
        )
        guard case let .refused(.activeIdentityChanged(expected, delivered)) = mismatch else {
            return XCTFail("expected active-identity refusal, got \(mismatch)")
        }
        XCTAssertEqual(expected, identity("wrong"))
        XCTAssertEqual(delivered, identity("a"))

        _ = await coordinator.requestNeighbor(descriptor("b"))
        _ = try await waitForTelemetry(coordinator) {
            $0.neighbor?.phase == .prepared
        }
        await coordinator.recordMemoryWarning()

        let telemetry = await coordinator.telemetry()
        XCTAssertNil(telemetry.neighbor)
        XCTAssertEqual(telemetry.memoryWarningCount, 1)
        XCTAssertTrue(telemetry.coarseMacroFallback)
        guard case let .memoryWarningCooldown(remaining)? = telemetry.lastRefusal else {
            return XCTFail("expected memory-warning cooldown telemetry")
        }
        XCTAssertGreaterThan(remaining, 0)
        let releasedIdentities = backend.releasedIdentities()
        XCTAssertEqual(releasedIdentities, [identity("b")])

        let refused = await coordinator.requestNeighbor(descriptor("c"))
        guard case .refused(.memoryWarningCooldown) = refused else {
            return XCTFail("expected cooldown refusal, got \(refused)")
        }
    }

    func testMemoryWarningDuringPreparationCannotStrandCancellation() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(
            activeIdentity: active.identity,
            delayedIdentities: [identity("b")]
        )
        let coordinator = makeCoordinator(active: active, backend: backend)

        _ = await coordinator.requestNeighbor(descriptor("b"))
        _ = try await waitForTelemetry(coordinator) {
            $0.neighbor?.identity == self.identity("b") &&
                $0.neighbor?.phase == .preparing
        }
        await coordinator.recordMemoryWarning()

        var telemetry = await coordinator.telemetry()
        XCTAssertEqual(telemetry.neighbor?.phase, .cancellingStaleDirection)
        XCTAssertNil(telemetry.neighbor?.replacement)
        XCTAssertEqual(telemetry.memoryWarningCount, 1)
        XCTAssertTrue(telemetry.coarseMacroFallback)

        await backend.completeDelayedPreparation(identity("b"))
        telemetry = try await waitForTelemetry(coordinator) {
            $0.neighbor == nil
        }
        XCTAssertEqual(telemetry.active, identity("a"))
        let cancelledIdentities = backend.cancelledIdentities()
        XCTAssertEqual(cancelledIdentities, [identity("b")])
        let releasedIdentities = backend.releasedIdentities()
        XCTAssertEqual(releasedIdentities, [identity("b")])
    }

    func testTelemetryDoesNotClaimUnsupportedPreparedAdoption() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(
            activeIdentity: active.identity,
            supportsPreparedAdoption: false
        )
        let coordinator = makeCoordinator(active: active, backend: backend)

        let telemetry = await coordinator.telemetry()
        XCTAssertFalse(telemetry.linkageReadyForAdoption)
        XCTAssertEqual(telemetry.residentWorldBytes, 100)
        let json = try await coordinator.telemetryJSON()
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
        )
        XCTAssertEqual(object["linkageReadyForAdoption"] as? Bool, false)
        XCTAssertEqual(object["residentWorldBytes"] as? Int, 100)
        let activeObject = try XCTUnwrap(object["active"] as? [String: Any])
        XCTAssertEqual(activeObject["city"] as? String, "test-city")
        XCTAssertEqual(activeObject["cell"] as? String, "a")
    }

    func testMismatchedPreparedPayloadIsReleasedAndNeverAdvertised() async throws {
        let active = TestCellPayload(identity: identity("a"), residentBytes: 100)
        let backend = TestCellBackend(
            activeIdentity: active.identity,
            mismatchedIdentities: [identity("b")]
        )
        let coordinator = makeCoordinator(active: active, backend: backend)

        _ = await coordinator.requestNeighbor(descriptor("b"))
        let telemetry = try await waitForTelemetry(coordinator) {
            $0.neighbor == nil && $0.lastFailure == "prepared payload identity mismatch"
        }
        XCTAssertEqual(telemetry.active, identity("a"))
        XCTAssertTrue(telemetry.coarseMacroFallback)
        let releasedIdentities = backend.releasedIdentities()
        XCTAssertEqual(releasedIdentities, [identity("mismatch")])
    }

    private func makeCoordinator(
        active: TestCellPayload,
        backend: TestCellBackend,
        thermalPressure: FightboxThermalPressure = .nominal,
        memorySample: FightboxCellMemorySample = FightboxCellMemorySample(
            advisoryReserveBytes: 1_000_000,
            processResidentBytes: 100,
            sampledUptimeSeconds: 1
        )
    ) -> FightboxCellStreamingCoordinator {
        FightboxCellStreamingCoordinator(
            active: active,
            backend: backend,
            memorySampler: FixedCellMemorySampler(memorySample),
            thermalSampler: FixedCellThermalSampler(thermalPressure),
            limits: FightboxCellStreamingLimits(
                minimumAdvisoryReserveBytes: 1_000,
                activePreparedTargetBytes: 10_000,
                preparationPeakLimitBytes: 20_000,
                rawCellHardLimitBytes: 1_000,
                memoryWarningCooldownSeconds: 60
            )
        )
    }

    private func descriptor(_ cell: String) -> FightboxCellDescriptor {
        FightboxCellDescriptor(
            identity: identity(cell),
            packageURL: URL(fileURLWithPath: "/tmp/\(cell).fightbox"),
            bakeURL: URL(fileURLWithPath: "/tmp/\(cell).baked"),
            programs: [],
            quality: .mobile,
            listenerPose: FightboxPose(
                position: .zero,
                forward: SIMD3<Float>(0, 1, 0),
                up: SIMD3<Float>(0, 0, 1)
            ),
            sources: [],
            estimate: FightboxCellPrepareEstimate(
                rawCellBytes: 10,
                preparedResidentBytes: 100,
                preparationScratchBytes: 10
            )
        )
    }

    private func identity(_ cell: String) -> FightboxCellIdentity {
        FightboxCellIdentity(city: "test-city", cell: cell)
    }

    private func waitForTelemetry(
        _ coordinator: FightboxCellStreamingCoordinator,
        timeoutSeconds: TimeInterval = 2,
        predicate: @escaping (FightboxCellStreamingTelemetry) -> Bool
    ) async throws -> FightboxCellStreamingTelemetry {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        repeat {
            let telemetry = await coordinator.telemetry()
            if predicate(telemetry) { return telemetry }
            try await Task.sleep(nanoseconds: 1_000_000)
        } while Date() < deadline
        throw CoordinatorTestError.timedOut
    }
}

private enum CoordinatorTestError: Error {
    case timedOut
}

private struct FixedCellThermalSampler: FightboxThermalSampling {
    let pressure: FightboxThermalPressure

    init(_ pressure: FightboxThermalPressure) {
        self.pressure = pressure
    }

    func sample() -> FightboxThermalPressure {
        pressure
    }
}

private final class FixedCellMemorySampler: FightboxCellMemorySampling,
    @unchecked Sendable
{
    private let value: FightboxCellMemorySample

    init(_ value: FightboxCellMemorySample) {
        self.value = value
    }

    func sample() throws -> FightboxCellMemorySample {
        value
    }
}

private final class TestCellPayload: FightboxPreparedCellPayload,
    @unchecked Sendable
{
    let identity: FightboxCellIdentity
    let residentBytes: UInt64

    init(identity: FightboxCellIdentity, residentBytes: UInt64) {
        self.identity = identity
        self.residentBytes = residentBytes
    }
}

private final class TestTailToken: FightboxTailRetirementToken,
    @unchecked Sendable
{
    let identity: FightboxCellIdentity

    init(identity: FightboxCellIdentity) {
        self.identity = identity
    }
}

private final class TestCellBackend: FightboxCellStreamingBackend,
    @unchecked Sendable
{
    let supportsPreparedAdoption: Bool

    private let lock = NSLock()
    private var activeIdentity: FightboxCellIdentity
    private let delayedIdentities: Set<FightboxCellIdentity>
    private let mismatchedIdentities: Set<FightboxCellIdentity>
    private var tailComplete = false
    private var delayedWaiters: [
        FightboxCellIdentity: CheckedContinuation<Void, Never>
    ] = [:]
    private var prepared: [FightboxCellIdentity] = []
    private var cancelled: [FightboxCellIdentity] = []
    private var released: [FightboxCellIdentity] = []
    private var finished: [FightboxCellIdentity] = []

    init(
        activeIdentity: FightboxCellIdentity,
        supportsPreparedAdoption: Bool = true,
        delayedIdentities: Set<FightboxCellIdentity> = [],
        mismatchedIdentities: Set<FightboxCellIdentity> = []
    ) {
        self.activeIdentity = activeIdentity
        self.supportsPreparedAdoption = supportsPreparedAdoption
        self.delayedIdentities = delayedIdentities
        self.mismatchedIdentities = mismatchedIdentities
    }

    func prepareCell(
        _ descriptor: FightboxCellDescriptor
    ) async throws -> any FightboxPreparedCellPayload {
        lock.withLock { prepared.append(descriptor.identity) }
        if delayedIdentities.contains(descriptor.identity) {
            await withCheckedContinuation { continuation in
                lock.withLock {
                    delayedWaiters[descriptor.identity] = continuation
                }
            }
        }
        let identity = mismatchedIdentities.contains(descriptor.identity)
            ? FightboxCellIdentity(city: descriptor.identity.city, cell: "mismatch")
            : descriptor.identity
        return TestCellPayload(
            identity: identity,
            residentBytes: descriptor.estimate.preparedResidentBytes
        )
    }

    func cancelPreparation(for identity: FightboxCellIdentity) {
        lock.withLock { cancelled.append(identity) }
    }

    func releasePrepared(_ payload: any FightboxPreparedCellPayload) {
        lock.withLock { released.append(payload.identity) }
    }

    func adoptPrepared(
        _ payload: any FightboxPreparedCellPayload
    ) throws -> any FightboxTailRetirementToken {
        lock.withLock {
            let retiring = TestTailToken(identity: activeIdentity)
            activeIdentity = payload.identity
            return retiring
        }
    }

    func isTailRetirementComplete(
        _: any FightboxTailRetirementToken
    ) throws -> Bool {
        lock.withLock { tailComplete }
    }

    func finishTailRetirement(
        _ token: any FightboxTailRetirementToken
    ) {
        lock.withLock { finished.append(token.identity) }
    }

    func completeDelayedPreparation(_ identity: FightboxCellIdentity) async {
        while true {
            let continuation = lock.withLock {
                delayedWaiters.removeValue(forKey: identity)
            }
            if let continuation {
                continuation.resume()
                return
            }
            await Task.yield()
        }
    }

    func setTailComplete(_ value: Bool) {
        lock.withLock { tailComplete = value }
    }

    func preparedIdentities() -> [FightboxCellIdentity] {
        lock.withLock { prepared }
    }

    func cancelledIdentities() -> [FightboxCellIdentity] {
        lock.withLock { cancelled }
    }

    func releasedIdentities() -> [FightboxCellIdentity] {
        lock.withLock { released }
    }

    func finishedTailIdentities() -> [FightboxCellIdentity] {
        lock.withLock { finished }
    }
}
