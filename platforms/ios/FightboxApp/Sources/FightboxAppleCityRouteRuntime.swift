import Foundation
import simd

@available(iOS 18.0, *)
struct FightboxRouteAdvanceOutcome: Sendable {
    let fromCellID: String
    let toCellID: String
    let activeCityPositionMm: SIMD2<Int64>
    let targetCityPositionMm: SIMD2<Int64>
}

/// App-owned orchestration for the real neutral session. Artifact verification,
/// preparation, and retirement remain off the audio callback; the callback only
/// observes the existing bounded world-swap channel.
@available(iOS 18.0, *)
@MainActor
final class FightboxAppleCityRouteRuntime {
    let installedRoute: FightboxInstalledCityRoute
    let coordinator: FightboxCellStreamingCoordinator
    let routeHost: FightboxCityRouteHost

    private let productionHost: FightboxAppleSpatialProductionHost
    private let pressureObserver: FightboxCellPressureObserver
    private let sourceCityPositionMm: SIMD3<Int64>
    private var listenerCityPositionMm: SIMD2<Int64>

    init(
        installedRoute: FightboxInstalledCityRoute,
        productionHost: FightboxAppleSpatialProductionHost
    ) throws {
        guard let firstCell = installedRoute.selector.manifest.cells.first,
              firstCell.cellId == installedRoute.initialArtifacts.cellId,
              firstCell.cityId == installedRoute.initialArtifacts.cityId
        else {
            throw FightboxAppleCityRouteRuntimeError.initialCellMismatch
        }
        let initial = FightboxPreparedNeutralCell(
            identity: FightboxCellIdentity(
                city: firstCell.cityId,
                cell: firstCell.cellId
            ),
            residentBytes: installedRoute.initialArtifacts.preparedResidentBytes,
            session: productionHost.session
        )
        let backend = FightboxNeutralSessionCellBackend(active: initial)
        let coordinator = FightboxCellStreamingCoordinator(
            active: initial,
            backend: backend
        )
        self.installedRoute = installedRoute
        self.productionHost = productionHost
        self.coordinator = coordinator
        routeHost = FightboxCityRouteHost(
            selector: installedRoute.selector,
            resolver: installedRoute.resolver,
            coordinator: coordinator
        )
        pressureObserver = FightboxCellPressureObserver(coordinator: coordinator)

        let offset = try Self.cellOffsetMm(firstCell)
        listenerCityPositionMm = SIMD2<Int64>(offset.x, offset.y)
        sourceCityPositionMm = SIMD3<Int64>(offset.x, offset.y + 2_000, offset.z)
    }

    var plannedTransitionCount: Int {
        max(0, installedRoute.selector.manifest.cells.count - 1)
    }

    func suspendExternalPositionControl() {
        productionHost.bodyMotionTracker.stop()
    }

    func resumeBodyTracking() throws {
        try productionHost.bodyMotionTracker.start(updateRateHz: 60)
    }

    func advanceAuthoredNeighbor() async throws -> FightboxRouteAdvanceOutcome? {
        try Task.checkCancellation()
        guard try await coordinator.refreshTailRetirement() else {
            throw FightboxAppleCityRouteRuntimeError.tailStillRetiring
        }
        let before = await coordinator.telemetry()
        guard let activeCell = installedRoute.selector.cell(id: before.active.cell) else {
            throw FightboxAppleCityRouteRuntimeError.unknownActiveCell(before.active.cell)
        }
        guard let targetCell = try installedRoute.selector.authoredNeighbor(
            activeCellId: activeCell.cellId,
            direction: .forward
        ) else {
            return nil
        }

        let crossing = try crossingPositions(from: activeCell, to: targetCell)
        let activePose = try listenerPose(
            cityPositionMm: crossing.activeSide,
            in: activeCell
        )
        let context = FightboxRouteDescriptorContext(
            programs: productionHost.session.programs,
            environmentalOrder: productionHost.session.environmentalOrder,
            quality: .mobile,
            defaultSourceLevelDB: 0,
            listenerPose: activePose,
            listenerCityPositionEnuMm: crossing.activeSide,
            listenerLinearVelocityMPS: .zero,
            sources: try sourceStates(in: activeCell)
        )
        let request = try await routeHost.requestAuthoredNeighbor(
            direction: .forward,
            context: context
        )
        switch request {
        case .noIncidentNeighbor:
            return nil
        case let .requested(cellId, result):
            guard cellId == targetCell.cellId else {
                throw FightboxAppleCityRouteRuntimeError.preparedWrongCell(
                    expected: targetCell.cellId,
                    delivered: cellId
                )
            }
            switch result {
            case .queued, .alreadyPending, .replacementPending:
                break
            case let .refused(reason):
                throw FightboxAppleCityRouteRuntimeError.preparationRefused(
                    String(describing: reason)
                )
            }
        }

        try await waitForPreparedNeighbor(cellID: targetCell.cellId)
        try Task.checkCancellation()
        try await coordinator.activatePreparedNeighbor()

        let targetPose = try listenerPose(
            cityPositionMm: crossing.targetSide,
            in: targetCell
        )
        productionHost.bodyMotionTracker.listenerPositionENU = targetPose.position
        try productionHost.session.updateListener(pose: targetPose)
        listenerCityPositionMm = crossing.targetSide
        try await waitForTailRetirement()

        return FightboxRouteAdvanceOutcome(
            fromCellID: activeCell.cellId,
            toCellID: targetCell.cellId,
            activeCityPositionMm: crossing.activeSide,
            targetCityPositionMm: crossing.targetSide
        )
    }

    func statusLine() async -> String {
        let telemetry = await coordinator.telemetry()
        let neighbor = telemetry.neighbor.map {
            "\($0.identity.cell) [\($0.phase.rawValue)]"
        } ?? "none"
        let retiring = telemetry.tailRetiring?.cell ?? "none"
        return "route \(installedRoute.selector.manifest.routeId) · " +
            "manifest \(installedRoute.manifestSHA256.prefix(12))… · " +
            "active \(telemetry.active.cell) · neighbor \(neighbor) · " +
            "retiring \(retiring)"
    }

    func telemetryJSON() async throws -> String {
        try await coordinator.telemetryJSON()
    }

    private func waitForPreparedNeighbor(cellID: String) async throws {
        let deadline = ProcessInfo.processInfo.systemUptime + 60
        while ProcessInfo.processInfo.systemUptime < deadline {
            try Task.checkCancellation()
            let telemetry = await coordinator.telemetry()
            if telemetry.neighbor == nil {
                if let failure = telemetry.lastFailure {
                    throw FightboxAppleCityRouteRuntimeError.preparationFailed(failure)
                }
                if let refusal = telemetry.lastRefusal {
                    throw FightboxAppleCityRouteRuntimeError.preparationRefused(
                        String(describing: refusal)
                    )
                }
            }
            if telemetry.neighbor?.identity.cell == cellID,
               telemetry.neighbor?.phase == .prepared
            {
                return
            }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        throw FightboxAppleCityRouteRuntimeError.preparationTimedOut(cellID)
    }

    private func waitForTailRetirement() async throws {
        let deadline = ProcessInfo.processInfo.systemUptime + 15
        while ProcessInfo.processInfo.systemUptime < deadline {
            try Task.checkCancellation()
            if try await coordinator.refreshTailRetirement() {
                return
            }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        throw FightboxAppleCityRouteRuntimeError.tailRetirementTimedOut
    }

    private func sourceStates(
        in cell: FightboxRouteCell
    ) throws -> [FightboxSpatialSourceState] {
        let offset = try Self.cellOffsetMm(cell)
        let local = SIMD3<Float>(
            Float(sourceCityPositionMm.x - offset.x) / 1_000,
            Float(sourceCityPositionMm.y - offset.y) / 1_000,
            Float(sourceCityPositionMm.z - offset.z) / 1_000
        )
        return productionHost.session.programs.indices.map { index in
            FightboxSpatialSourceState(
                active: index == 0,
                pose: FightboxPose(
                    position: index == 0 ? local : .zero,
                    forward: SIMD3<Float>(0, -1, 0),
                    up: SIMD3<Float>(0, 0, 1)
                )
            )
        }
    }

    private func listenerPose(
        cityPositionMm: SIMD2<Int64>,
        in cell: FightboxRouteCell
    ) throws -> FightboxPose {
        let offset = try Self.cellOffsetMm(cell)
        return FightboxPose(
            position: SIMD3<Float>(
                Float(cityPositionMm.x - offset.x) / 1_000,
                Float(cityPositionMm.y - offset.y) / 1_000,
                0
            ),
            forward: SIMD3<Float>(0, 1, 0),
            up: SIMD3<Float>(0, 0, 1)
        )
    }

    private func crossingPositions(
        from active: FightboxRouteCell,
        to target: FightboxRouteCell
    ) throws -> (activeSide: SIMD2<Int64>, targetSide: SIMD2<Int64>) {
        let eastDelta = target.gridIndex.east - active.gridIndex.east
        let northDelta = target.gridIndex.north - active.gridIndex.north
        let activeOffset = try Self.cellOffsetMm(active)
        let otherCoordinate = SIMD2<Int64>(activeOffset.x, activeOffset.y)
        let activeSide: SIMD2<Int64>
        let targetSide: SIMD2<Int64>
        switch (eastDelta, northDelta) {
        case (1, 0):
            activeSide = SIMD2(active.selection.ownershipBoundsCityEnuMm.max[0] - 1, otherCoordinate.y)
            targetSide = SIMD2(active.selection.ownershipBoundsCityEnuMm.max[0], otherCoordinate.y)
        case (-1, 0):
            activeSide = SIMD2(active.selection.ownershipBoundsCityEnuMm.min[0], otherCoordinate.y)
            targetSide = SIMD2(active.selection.ownershipBoundsCityEnuMm.min[0] - 1, otherCoordinate.y)
        case (0, 1):
            activeSide = SIMD2(otherCoordinate.x, active.selection.ownershipBoundsCityEnuMm.max[1] - 1)
            targetSide = SIMD2(otherCoordinate.x, active.selection.ownershipBoundsCityEnuMm.max[1])
        case (0, -1):
            activeSide = SIMD2(otherCoordinate.x, active.selection.ownershipBoundsCityEnuMm.min[1])
            targetSide = SIMD2(otherCoordinate.x, active.selection.ownershipBoundsCityEnuMm.min[1] - 1)
        default:
            throw FightboxAppleCityRouteRuntimeError.nonAdjacentTransition(
                from: active.cellId,
                to: target.cellId
            )
        }
        guard try installedRoute.selector.ownerCell(
            eastMm: activeSide.x,
            northMm: activeSide.y
        )?.cellId == active.cellId,
        try installedRoute.selector.ownerCell(
            eastMm: targetSide.x,
            northMm: targetSide.y
        )?.cellId == target.cellId
        else {
            throw FightboxAppleCityRouteRuntimeError.invalidOwnershipCrossing(
                from: active.cellId,
                to: target.cellId
            )
        }
        return (activeSide, targetSide)
    }

    private static func cellOffsetMm(
        _ cell: FightboxRouteCell
    ) throws -> SIMD3<Int64> {
        guard cell.localToCityEnuM.count == 3,
              cell.localToCityEnuM.allSatisfy(\.isFinite)
        else {
            throw FightboxAppleCityRouteRuntimeError.invalidCellOffset(cell.cellId)
        }
        return SIMD3<Int64>(
            Int64((cell.localToCityEnuM[0] * 1_000).rounded()),
            Int64((cell.localToCityEnuM[1] * 1_000).rounded()),
            Int64((cell.localToCityEnuM[2] * 1_000).rounded())
        )
    }
}

@available(iOS 18.0, *)
enum FightboxAppleCityRouteRuntimeError: Error, CustomStringConvertible {
    case initialCellMismatch
    case unknownActiveCell(String)
    case invalidCellOffset(String)
    case nonAdjacentTransition(from: String, to: String)
    case invalidOwnershipCrossing(from: String, to: String)
    case preparedWrongCell(expected: String, delivered: String)
    case preparationRefused(String)
    case preparationFailed(String)
    case preparationTimedOut(String)
    case tailStillRetiring
    case tailRetirementTimedOut

    var description: String {
        switch self {
        case .initialCellMismatch:
            return "The verified initial artifact does not match the route's first cell"
        case let .unknownActiveCell(cell):
            return "Coordinator reported unknown active cell \(cell)"
        case let .invalidCellOffset(cell):
            return "Route cell \(cell) has an invalid city offset"
        case let .nonAdjacentTransition(from, to):
            return "Authored route transition is not adjacent: \(from) → \(to)"
        case let .invalidOwnershipCrossing(from, to):
            return "No exact half-open ownership crossing exists for \(from) → \(to)"
        case let .preparedWrongCell(expected, delivered):
            return "Expected prepared cell \(expected), received \(delivered)"
        case let .preparationRefused(detail):
            return "Neighbor preparation refused: \(detail)"
        case let .preparationFailed(detail):
            return "Neighbor preparation failed: \(detail)"
        case let .preparationTimedOut(cell):
            return "Neighbor preparation timed out for \(cell)"
        case .tailStillRetiring:
            return "The previous world still owns an admitted environmental tail"
        case .tailRetirementTimedOut:
            return "The retired world's environmental tail did not complete within 15 seconds"
        }
    }
}
