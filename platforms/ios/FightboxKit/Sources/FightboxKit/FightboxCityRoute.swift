import Foundation

public enum FightboxRouteManifestError: Error, Equatable, CustomStringConvertible, Sendable {
    case invalid(String)

    public var description: String {
        switch self {
        case .invalid(let message): message
        }
    }
}

public struct FightboxRouteGridIndex: Codable, Equatable, Hashable, Sendable {
    public let east: Int32
    public let north: Int32
}

public struct FightboxRouteBoundsMm: Codable, Equatable, Sendable {
    public let min: [Int64]
    public let max: [Int64]

    public func containsHalfOpen(eastMm: Int64, northMm: Int64) -> Bool {
        min[0] <= eastMm && eastMm < max[0] && min[1] <= northMm && northMm < max[1]
    }

    fileprivate func validate(_ label: String) throws {
        guard min.count == 2, max.count == 2, min[0] < max[0], min[1] < max[1] else {
            throw FightboxRouteManifestError.invalid("\(label) must contain two increasing axes")
        }
    }
}

public struct FightboxRouteGridPolicy: Codable, Equatable, Sendable {
    public let probeFootprintM: UInt32
    public let strideM: UInt32
    public let pairwiseOverlapM: UInt32
    public let ownershipGuardM: UInt32
    public let geometryHaloM: UInt32
    public let maximumResidentWorlds: UInt32
    public let maximumPreparedNeighbors: UInt32
    public let maximumResidentEchoAuthorityCells: UInt32
    public let echoAuthorityResidentCapBytes: UInt64
}

public struct FightboxRouteWorldBinding: Codable, Equatable, Sendable {
    public let manifestSha256: String
    public let meshSha256: String
    public let materialsSha256: String
}

public struct FightboxRouteCompletedBakeBinding: Codable, Equatable, Sendable {
    public let sidecarSha256: String
    public let probeBatchSha256: String
    public let probeBatchSizeBytes: UInt64
    public let pathDataSizeBytes: UInt64
    public let probeByteEstimateV2Sha256: String?
    public let probeByteEstimateV2SizeBytes: UInt64?
}

public enum FightboxRouteBakeState: String, Codable, Equatable, Sendable {
    case probePlan = "probe_plan"
    case bakedProbeBatch = "baked_probe_batch"
}

public enum FightboxAboveMaximumLayerPolicy: String, Codable, Equatable, Sendable {
    case directReflectionsOnly = "direct_reflections_only"
}

public struct FightboxCalibratedTierCounts: Codable, Equatable, Sendable {
    public let ownerHomeCount: UInt64
    public let routeCoreCount: UInt64
    public let transitionCount: UInt64
    public let residualCount: UInt64
}

public struct FightboxCalibratedMeshObservable: Codable, Equatable, Sendable {
    public let algorithm: String
    public let coordinateEncoding: String
    public let meshSha256: String?
    public let ownerGroundHeightMm: Int64
    public let ownerGroundProbeCount: UInt64
    public let wallEdgeCount: UInt64
    public let blockedOwnerGroundOrderedPairCount: UInt64
    public let openOwnerGroundOrderedPairCount: UInt64
}

public struct FightboxCalibratedModel: Codable, Equatable, Sendable {
    public let revision: String
    public let fixedBytes: UInt64
    public let ownerHomeBytesPerProbe: UInt64
    public let routeCoreBytesPerProbe: UInt64
    public let transitionBytesPerProbe: UInt64
    public let residualBytesPerProbe: UInt64
    public let openOwnerGroundOrderedPairBytes: UInt64
    public let ownerHomeProbeCount: UInt64
    public let routeCoreProbeCount: UInt64
    public let transitionProbeCount: UInt64
    public let residualProbeCount: UInt64
    public let ownerGroundProbeCount: UInt64
    public let openOwnerGroundOrderedPairCount: UInt64
    public let pointEstimateBytes: UInt64
    public let projectedLowBytes: UInt64
    public let projectedHighBytes: UInt64
    public let reservationBytes: UInt64
    public let meshObservable: FightboxCalibratedMeshObservable
}

public struct FightboxRouteBakeBinding: Codable, Equatable, Sendable {
    public let state: FightboxRouteBakeState
    public let probePlanSidecarSha256: String
    public let placementPolicySha256: String
    public let probeLayoutSha256: String
    public let probeCount: UInt64
    public let tierIds: [String]
    public let maximumLayerM: UInt32
    public let aboveMaximumLayer: FightboxAboveMaximumLayerPolicy
    public let calibratedModel: FightboxCalibratedModel?
    public let completed: FightboxRouteCompletedBakeBinding?
}

public struct FightboxRouteEchoAuthorityBinding: Codable, Equatable, Sendable {
    public let capability: String
    public let packageSidecarPath: String
    public let contentSha256: String
    public let serializedSizeBytes: UInt64
    public let residentSizeBytes: UInt64
    public let anchorSetSha256: String
    public let listenerLayoutSha256: String
    public let coordinateFrameKey: String
    public let staticAnchorCount: UInt32
    public let residencyScope: String
}

public struct FightboxRouteSelectionMetadata: Codable, Equatable, Sendable {
    public let routeOffsetMm: Int64
    public let ownershipBoundsCityEnuMm: FightboxRouteBoundsMm
    public let guardBoundsCityEnuMm: FightboxRouteBoundsMm
}

public struct FightboxRoutePrefetchMetadata: Codable, Equatable, Sendable {
    public let reverseCellId: String?
    public let forwardCellId: String?
    public let rawCellBytes: UInt64
    public let preparedResidentEstimateBytes: UInt64
    public let preparationScratchBytes: UInt64
    public let echoAuthorityResidentBytes: UInt64
    public let echoAuthorityLoadPolicy: String
    public let estimateRevision: String
}

public struct FightboxRouteInstalledSize: Codable, Equatable, Sendable {
    public let packageBytes: UInt64
    public let bakedArtifactBytes: UInt64
    public let actualInstalledBytes: UInt64
    public let projectedRemainingBakeBytes: UInt64
    public let projectedCompleteInstalledBytes: UInt64
}

public struct FightboxRouteCell: Codable, Equatable, Sendable {
    public let sequence: UInt32
    public let cityId: String
    public let cellId: String
    public let gridIndex: FightboxRouteGridIndex
    public let localToCityEnuM: [Double]
    public let probeFootprintBoundsCityEnuMm: FightboxRouteBoundsMm
    public let geometryHaloBoundsCityEnuMm: FightboxRouteBoundsMm
    public let world: FightboxRouteWorldBinding
    public let cityBake: FightboxRouteBakeBinding
    public let echoAuthority: FightboxRouteEchoAuthorityBinding?
    public let selection: FightboxRouteSelectionMetadata
    public let prefetch: FightboxRoutePrefetchMetadata
    public let installedSize: FightboxRouteInstalledSize
}

public enum FightboxRouteAdjacencyAxis: String, Codable, Equatable, Sendable {
    case eastWest = "east_west"
    case northSouth = "north_south"
}

public struct FightboxRouteAdjacency: Codable, Equatable, Sendable {
    public let id: String
    public let cells: [String]
    public let axis: FightboxRouteAdjacencyAxis
    public let overlapBoundsCityEnuMm: FightboxRouteBoundsMm
    public let ownershipSwitchCoordinateMm: Int64
    public let overlapProbeCount: UInt64
    public let overlapProbesSha256: String
}

public struct FightboxOwnerHomeDesignation: Codable, Equatable, Sendable {
    public let cellId: String
    public let tierId: String
    public let probeCount: UInt64
}

public struct FightboxFourCellFixture: Codable, Equatable, Sendable {
    public let state: String
    public let bakesLaunched: Bool
    public let gridMin: FightboxRouteGridIndex
    public let gridMax: FightboxRouteGridIndex
    public let streamedUnionBoundsCityEnuMm: FightboxRouteBoundsMm
    public let monolithicOracleBoundsCityEnuMm: FightboxRouteBoundsMm
    public let monolithicOraclePathRangeM: UInt32
    public let streamedCellBakesRequired: UInt32
    public let monolithicOracleBakesRequired: UInt32
    public let seamIds: [String]
}

public struct FightboxRouteInstalledTotals: Codable, Equatable, Sendable {
    public let actualInstalledBytes: UInt64
    public let projectedRemainingBakeBytes: UInt64
    public let projectedCompleteInstalledBytes: UInt64
    public let completedCellCount: UInt32
    public let plannedCellCount: UInt32
}

public struct FightboxCityRouteManifest: Codable, Equatable, Sendable {
    public let schemaVersion: String
    public let assemblerRevision: String
    /// Absent/false denotes frozen evidence-only route authority.
    public let productionEligible: Bool?
    public let cityId: String
    public let routeId: String
    public let routeOrder: String
    public let gridPolicy: FightboxRouteGridPolicy
    public let cells: [FightboxRouteCell]
    public let adjacencies: [FightboxRouteAdjacency]
    public let ownerHome: FightboxOwnerHomeDesignation?
    public let fourCellFixture: FightboxFourCellFixture?
    public let installedTotals: FightboxRouteInstalledTotals

    public static func decodeStrict(_ data: Data) throws -> Self {
        let raw = try JSONSerialization.jsonObject(with: data)
        try FightboxRouteStrictShape.validate(raw)
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            let manifest = try decoder.decode(Self.self, from: data)
            try manifest.validate()
            return manifest
        } catch let error as FightboxRouteManifestError {
            throw error
        } catch {
            throw FightboxRouteManifestError.invalid("city route JSON decode failed: \(error)")
        }
    }

    public func validate() throws {
        guard schemaVersion == "fightbox.city-route.v1" else {
            throw FightboxRouteManifestError.invalid("unsupported city route schema \(schemaVersion)")
        }
        guard assemblerRevision == "global-slice-route-assembly-v1" else {
            throw FightboxRouteManifestError.invalid("unsupported route assembler \(assemblerRevision)")
        }
        guard routeOrder == "authored_sequence_at_485m_stride",
              Self.isStableId(cityId), Self.isStableId(routeId) else {
            throw FightboxRouteManifestError.invalid("route ordering or stable identity is invalid")
        }
        guard gridPolicy.probeFootprintM == 585,
              gridPolicy.strideM == 485,
              gridPolicy.pairwiseOverlapM == 100,
              gridPolicy.ownershipGuardM == 50,
              gridPolicy.geometryHaloM == 600,
              gridPolicy.maximumResidentWorlds == 2,
              gridPolicy.maximumPreparedNeighbors == 1,
              gridPolicy.maximumResidentEchoAuthorityCells == 2,
              gridPolicy.echoAuthorityResidentCapBytes == 67_108_864 else {
            throw FightboxRouteManifestError.invalid("route grid policy differs from the frozen two-world contract")
        }
        guard !cells.isEmpty else {
            throw FightboxRouteManifestError.invalid("city route contains no cells")
        }

        var cellIds = Set<String>()
        var actualInstalled: UInt64 = 0
        var projectedRemaining: UInt64 = 0
        var completedCount: UInt32 = 0
        for (offset, cell) in cells.enumerated() {
            let centerEast = Int64(cell.gridIndex.east) * 485_000
            let centerNorth = Int64(cell.gridIndex.north) * 485_000
            let expectedFootprint = FightboxRouteBoundsMm(
                min: [centerEast - 292_500, centerNorth - 292_500],
                max: [centerEast + 292_500, centerNorth + 292_500]
            )
            let expectedOwnership = FightboxRouteBoundsMm(
                min: [centerEast - 242_500, centerNorth - 242_500],
                max: [centerEast + 242_500, centerNorth + 242_500]
            )
            let expectedGeometry = FightboxRouteBoundsMm(
                min: [centerEast - 892_500, centerNorth - 892_500],
                max: [centerEast + 892_500, centerNorth + 892_500]
            )
            let expectedId = "\(cityId):e\(cell.gridIndex.east):n\(cell.gridIndex.north)"
            guard cell.sequence == UInt32(offset), cell.cityId == cityId,
                  cell.cellId == expectedId, cellIds.insert(cell.cellId).inserted,
                  cell.localToCityEnuM == [
                    Double(cell.gridIndex.east) * 485.0,
                    Double(cell.gridIndex.north) * 485.0,
                    0.0,
                  ],
                  cell.probeFootprintBoundsCityEnuMm == expectedFootprint,
                  cell.selection.ownershipBoundsCityEnuMm == expectedOwnership,
                  cell.selection.guardBoundsCityEnuMm == expectedFootprint,
                  cell.geometryHaloBoundsCityEnuMm == expectedGeometry,
                  cell.selection.routeOffsetMm == Int64(offset) * 485_000 else {
                throw FightboxRouteManifestError.invalid("route cell order, identity, transform, or bounds are invalid")
            }
            try expectedFootprint.validate("probe footprint for \(cell.cellId)")
            try expectedGeometry.validate("geometry halo for \(cell.cellId)")
            try validateSha256(cell.world.manifestSha256, "world manifest")
            try validateSha256(cell.world.meshSha256, "mesh")
            try validateSha256(cell.world.materialsSha256, "materials")
            try validateSha256(cell.cityBake.probePlanSidecarSha256, "probe plan")
            try validateSha256(cell.cityBake.placementPolicySha256, "placement policy")
            try validateSha256(cell.cityBake.probeLayoutSha256, "probe layout")
            guard cell.cityBake.probeCount > 0,
                  cell.cityBake.maximumLayerM == 63,
                  cell.cityBake.aboveMaximumLayer == .directReflectionsOnly,
                  !cell.cityBake.tierIds.isEmpty,
                  cell.prefetch.rawCellBytes > 0,
                  cell.prefetch.preparedResidentEstimateBytes > 0,
                  cell.prefetch.preparationScratchBytes == cell.prefetch.rawCellBytes,
                  cell.prefetch.estimateRevision == "probe-bytes-x3.29-plus-package-plus-cell-echo-v2",
                  cell.installedSize.packageBytes > 0 else {
                throw FightboxRouteManifestError.invalid("cell \(cell.cellId) stream authority is invalid")
            }
            if let model = cell.cityBake.calibratedModel {
                try Self.validateCalibratedModel(model, probeCount: cell.cityBake.probeCount, meshSha256: cell.world.meshSha256, cellId: cell.cellId)
            }
            let expectedReverse = offset == 0 ? nil : cells[offset - 1].cellId
            let expectedForward = offset + 1 == cells.count ? nil : cells[offset + 1].cellId
            guard cell.prefetch.reverseCellId == expectedReverse,
                  cell.prefetch.forwardCellId == expectedForward else {
                throw FightboxRouteManifestError.invalid("cell \(cell.cellId) prefetch order differs from the route")
            }

            switch (cell.cityBake.state, cell.cityBake.completed) {
            case (.probePlan, nil):
                guard cell.installedSize.bakedArtifactBytes == 0,
                      cell.installedSize.projectedRemainingBakeBytes > 0 else {
                    throw FightboxRouteManifestError.invalid("probe-plan cell has invalid installed size state")
                }
            case (.bakedProbeBatch, .some(let completed)):
                try validateSha256(completed.sidecarSha256, "completed bake sidecar")
                try validateSha256(completed.probeBatchSha256, "completed probe batch")
                guard completed.probeBatchSizeBytes > 0,
                      completed.pathDataSizeBytes > 0,
                      completed.probeBatchSizeBytes == cell.prefetch.rawCellBytes,
                      cell.installedSize.bakedArtifactBytes > 0,
                      cell.installedSize.projectedRemainingBakeBytes == 0 else {
                    throw FightboxRouteManifestError.invalid("baked cell \(cell.cellId) lacks exact completed identity")
                }
                switch (completed.probeByteEstimateV2Sha256, completed.probeByteEstimateV2SizeBytes) {
                case (.none, .none):
                    guard productionEligible != true else {
                        throw FightboxRouteManifestError.invalid(
                            "production route cell \(cell.cellId) lacks estimator identity"
                        )
                    }
                case (.some(let digest), .some(let size)):
                    guard productionEligible == true else {
                        throw FightboxRouteManifestError.invalid(
                            "evidence-only route cell \(cell.cellId) carries production estimator identity"
                        )
                    }
                    try validateSha256(digest, "probe-byte estimate v2")
                    guard size > 0, cell.installedSize.bakedArtifactBytes >= size else {
                        throw FightboxRouteManifestError.invalid("baked cell \(cell.cellId) has invalid estimator identity")
                    }
                default:
                    throw FightboxRouteManifestError.invalid("baked cell \(cell.cellId) has a partial estimator identity")
                }
                completedCount += 1
            default:
                throw FightboxRouteManifestError.invalid("cell bake state and completed binding disagree")
            }

            if let echo = cell.echoAuthority {
                try validateSha256(echo.contentSha256, "echo authority content")
                try validateSha256(echo.anchorSetSha256, "echo authority anchor set")
                try validateSha256(echo.listenerLayoutSha256, "echo authority listener layout")
                guard echo.capability == "fightbox.echo-authority.v1",
                      echo.packageSidecarPath == "echo-authority.bin",
                      echo.residencyScope == "active_or_prepared_cell_only",
                      echo.serializedSizeBytes > 0, echo.serializedSizeBytes <= 67_108_864,
                      echo.residentSizeBytes > 0, echo.residentSizeBytes <= 67_108_864,
                      echo.staticAnchorCount > 0,
                      echo.coordinateFrameKey.utf8.count == 32,
                      Self.isLowerHex(echo.coordinateFrameKey),
                      cell.prefetch.echoAuthorityResidentBytes == echo.residentSizeBytes,
                      cell.prefetch.echoAuthorityLoadPolicy == "active_or_prepared_cell_only" else {
                    throw FightboxRouteManifestError.invalid("cell \(cell.cellId) echo authority is invalid")
                }
            } else {
                guard cell.prefetch.echoAuthorityResidentBytes == 0,
                      cell.prefetch.echoAuthorityLoadPolicy == "not_installed" else {
                    throw FightboxRouteManifestError.invalid("echo-free cell carries authority residency metadata")
                }
            }

            let (cellActual, actualOverflow) = cell.installedSize.packageBytes.addingReportingOverflow(
                cell.installedSize.bakedArtifactBytes
            )
            let (cellProjected, projectedOverflow) = cellActual.addingReportingOverflow(
                cell.installedSize.projectedRemainingBakeBytes
            )
            guard !actualOverflow, !projectedOverflow,
                  cell.installedSize.actualInstalledBytes == cellActual,
                  cell.installedSize.projectedCompleteInstalledBytes == cellProjected else {
                throw FightboxRouteManifestError.invalid("cell \(cell.cellId) installed size fields disagree")
            }
            let (newActual, totalActualOverflow) = actualInstalled.addingReportingOverflow(cellActual)
            let (newRemaining, totalRemainingOverflow) = projectedRemaining.addingReportingOverflow(
                cell.installedSize.projectedRemainingBakeBytes
            )
            guard !totalActualOverflow, !totalRemainingOverflow else {
                throw FightboxRouteManifestError.invalid("route installed totals overflow")
            }
            actualInstalled = newActual
            projectedRemaining = newRemaining
        }

        for pair in zip(cells, cells.dropFirst()) {
            guard Self.manhattan(pair.0.gridIndex, pair.1.gridIndex) == 1 else {
                throw FightboxRouteManifestError.invalid("consecutive route cells are not grid-adjacent")
            }
            let leftEcho = pair.0.echoAuthority?.residentSizeBytes ?? 0
            let rightEcho = pair.1.echoAuthority?.residentSizeBytes ?? 0
            let (residentPair, overflow) = leftEcho.addingReportingOverflow(rightEcho)
            guard !overflow, residentPair <= 67_108_864 else {
                throw FightboxRouteManifestError.invalid("active/prepared echo authority pair exceeds 64 MiB")
            }
        }

        var expectedAdjacencyIds = Set<String>()
        for leftIndex in cells.indices {
            for rightIndex in cells.indices where rightIndex > leftIndex {
                if Self.manhattan(cells[leftIndex].gridIndex, cells[rightIndex].gridIndex) == 1 {
                    let ids = [cells[leftIndex].cellId, cells[rightIndex].cellId].sorted()
                    expectedAdjacencyIds.insert("\(ids[0])--\(ids[1])")
                }
            }
        }
        guard adjacencies.count == expectedAdjacencyIds.count else {
            throw FightboxRouteManifestError.invalid("route adjacency count differs from its cell grid")
        }
        var deliveredAdjacencyIds = Set<String>()
        for adjacency in adjacencies {
            guard adjacency.cells.count == 2 else {
                throw FightboxRouteManifestError.invalid("route adjacency must contain two cell IDs")
            }
            let ids = adjacency.cells.sorted()
            guard adjacency.cells == ids,
                  adjacency.id == "\(ids[0])--\(ids[1])",
                  expectedAdjacencyIds.contains(adjacency.id),
                  deliveredAdjacencyIds.insert(adjacency.id).inserted,
                  let left = cells.first(where: { $0.cellId == ids[0] }),
                  let right = cells.first(where: { $0.cellId == ids[1] }),
                  Self.manhattan(left.gridIndex, right.gridIndex) == 1,
                  adjacency.overlapProbeCount > 0 else {
                throw FightboxRouteManifestError.invalid("route adjacency identity is invalid")
            }
            try validateSha256(adjacency.overlapProbesSha256, "overlap probes")
            guard let overlap = Self.intersection(
                left.probeFootprintBoundsCityEnuMm,
                right.probeFootprintBoundsCityEnuMm
            ) else {
                throw FightboxRouteManifestError.invalid("route adjacency has no overlap")
            }
            let expectedAxis: FightboxRouteAdjacencyAxis = left.gridIndex.east != right.gridIndex.east
                ? .eastWest : .northSouth
            let expectedSwitch: Int64
            if expectedAxis == .eastWest {
                let west = left.gridIndex.east < right.gridIndex.east ? left : right
                expectedSwitch = west.selection.ownershipBoundsCityEnuMm.max[0]
            } else {
                let south = left.gridIndex.north < right.gridIndex.north ? left : right
                expectedSwitch = south.selection.ownershipBoundsCityEnuMm.max[1]
            }
            guard adjacency.axis == expectedAxis,
                  adjacency.overlapBoundsCityEnuMm == overlap,
                  adjacency.ownershipSwitchCoordinateMm == expectedSwitch else {
                throw FightboxRouteManifestError.invalid("route adjacency geometry differs from its cells")
            }
        }

        if let ownerHome {
            guard ownerHome.tierId == "owner-home", ownerHome.probeCount > 0,
                  let ownerCell = cells.first(where: { $0.cellId == ownerHome.cellId }),
                  ownerCell.cityBake.tierIds.contains("owner-home") else {
                throw FightboxRouteManifestError.invalid("owner-home designation is invalid")
            }
        }
        guard (fourCellFixture != nil) == (cells.count == 4) else {
            throw FightboxRouteManifestError.invalid("four-cell fixture presence differs from route size")
        }
        if let fixture = fourCellFixture {
            let minEast = cells.map(\.gridIndex.east).min()!
            let maxEast = cells.map(\.gridIndex.east).max()!
            let minNorth = cells.map(\.gridIndex.north).min()!
            let maxNorth = cells.map(\.gridIndex.north).max()!
            let expectedGrid = Set([
                FightboxRouteGridIndex(east: minEast, north: minNorth),
                FightboxRouteGridIndex(east: maxEast, north: minNorth),
                FightboxRouteGridIndex(east: minEast, north: maxNorth),
                FightboxRouteGridIndex(east: maxEast, north: maxNorth),
            ])
            guard maxEast - minEast == 1, maxNorth - minNorth == 1,
                  Set(cells.map(\.gridIndex)) == expectedGrid,
                  deliveredAdjacencyIds.count == 4 else {
                throw FightboxRouteManifestError.invalid("four-cell fixture is not one contiguous 2x2 grid")
            }
            let streamed = Self.union(cells.map(\.probeFootprintBoundsCityEnuMm))
            let oracle = FightboxRouteBoundsMm(
                min: [streamed.min[0] - 50_000, streamed.min[1] - 50_000],
                max: [streamed.max[0] + 50_000, streamed.max[1] + 50_000]
            )
            let completed = cells.filter { $0.cityBake.state == .bakedProbeBatch }.count
            let expectedState = completed == 0 ? "plan_only"
                : completed == 4 ? "streamed_cell_bakes_complete_oracle_pending"
                : "streamed_cell_bakes_incomplete"
            guard fixture.state == expectedState,
                  fixture.bakesLaunched == (completed != 0),
                  fixture.gridMin == FightboxRouteGridIndex(east: minEast, north: minNorth),
                  fixture.gridMax == FightboxRouteGridIndex(east: maxEast, north: maxNorth),
                  fixture.streamedUnionBoundsCityEnuMm == streamed,
                  fixture.monolithicOracleBoundsCityEnuMm == oracle,
                  oracle.max[0] - oracle.min[0] == 1_170_000,
                  oracle.max[1] - oracle.min[1] == 1_170_000,
                  fixture.monolithicOraclePathRangeM == 1_750,
                  fixture.streamedCellBakesRequired == 4,
                  fixture.monolithicOracleBakesRequired == 1,
                  fixture.seamIds == adjacencies.map(\.id) else {
                throw FightboxRouteManifestError.invalid("four-cell oracle contract is invalid")
            }
        }

        let (projectedInstalled, projectedOverflow) = actualInstalled.addingReportingOverflow(
            projectedRemaining
        )
        guard !projectedOverflow,
              installedTotals.actualInstalledBytes == actualInstalled,
              installedTotals.projectedRemainingBakeBytes == projectedRemaining,
              installedTotals.projectedCompleteInstalledBytes == projectedInstalled,
              installedTotals.completedCellCount == completedCount,
              installedTotals.plannedCellCount == UInt32(cells.count) else {
            throw FightboxRouteManifestError.invalid("route installed totals disagree with cells")
        }
    }

    private static func validateCalibratedModel(
        _ model: FightboxCalibratedModel,
        probeCount: UInt64,
        meshSha256: String,
        cellId: String
    ) throws {
        guard model.revision == "wave17-fixed-tier-mesh-open-pairs-v2",
              model.fixedBytes == 6_000,
              model.ownerHomeBytesPerProbe == 3_315,
              model.routeCoreBytesPerProbe == 31,
              model.transitionBytesPerProbe == 54,
              model.residualBytesPerProbe == 30,
              model.openOwnerGroundOrderedPairBytes == 14,
              model.ownerGroundProbeCount == model.ownerHomeProbeCount,
              model.meshObservable.ownerGroundProbeCount == model.ownerGroundProbeCount,
              model.meshObservable.algorithm == "canonical-package-mesh-owner-ground-open-pairs-v2",
              model.meshObservable.coordinateEncoding == "signed_integer_millimetres",
              model.meshObservable.meshSha256 == meshSha256,
              model.meshObservable.ownerGroundHeightMm == 1_500 else {
            throw FightboxRouteManifestError.invalid("calibrated model coefficients or observable are invalid for \(cellId)")
        }
        let (tier01, overflow01) = model.ownerHomeProbeCount.addingReportingOverflow(model.routeCoreProbeCount)
        let (tier23, overflow23) = model.transitionProbeCount.addingReportingOverflow(model.residualProbeCount)
        let (tierTotal, overflowTotal) = tier01.addingReportingOverflow(tier23)
        guard !overflow01, !overflow23, !overflowTotal, tierTotal == probeCount else {
            throw FightboxRouteManifestError.invalid("calibrated tier counts do not bind probe count")
        }
        let pairFactor = model.ownerGroundProbeCount == 0
            ? 0 : model.ownerGroundProbeCount - 1
        let (pairs, pairOverflow) = model.ownerGroundProbeCount.multipliedReportingOverflow(
            by: pairFactor
        )
        let (accounted, accountedOverflow) = model.meshObservable.blockedOwnerGroundOrderedPairCount.addingReportingOverflow(model.openOwnerGroundOrderedPairCount)
        guard !pairOverflow, !accountedOverflow, pairs == accounted,
              model.openOwnerGroundOrderedPairCount == model.meshObservable.openOwnerGroundOrderedPairCount else {
            throw FightboxRouteManifestError.invalid("calibrated mesh pair accounting is invalid")
        }
        let (ownerBytes, o1) = model.ownerHomeProbeCount.multipliedReportingOverflow(by: model.ownerHomeBytesPerProbe)
        let (routeBytes, o2) = model.routeCoreProbeCount.multipliedReportingOverflow(by: model.routeCoreBytesPerProbe)
        let (transitionBytes, o3) = model.transitionProbeCount.multipliedReportingOverflow(by: model.transitionBytesPerProbe)
        let (residualBytes, o4) = model.residualProbeCount.multipliedReportingOverflow(by: model.residualBytesPerProbe)
        let (openBytes, o5) = model.openOwnerGroundOrderedPairCount.multipliedReportingOverflow(by: model.openOwnerGroundOrderedPairBytes)
        var point = model.fixedBytes
        for (value, overflow) in [(ownerBytes, o1), (routeBytes, o2), (transitionBytes, o3), (residualBytes, o4), (openBytes, o5)] {
            let (next, addOverflow) = point.addingReportingOverflow(value)
            guard !overflow, !addOverflow else { throw FightboxRouteManifestError.invalid("calibrated model arithmetic overflows for \(cellId)") }
            point = next
        }
        let (lowProduct, lowOverflow) = point.multipliedReportingOverflow(by: 70)
        let (highProduct, highOverflow) = point.multipliedReportingOverflow(by: 130)
        guard !lowOverflow, !highOverflow else { throw FightboxRouteManifestError.invalid("calibrated model band arithmetic overflows for \(cellId)") }
        let highPlus = highProduct.addingReportingOverflow(99)
        guard !highPlus.overflow, point == model.pointEstimateBytes,
              model.projectedLowBytes == lowProduct / 100,
              model.projectedHighBytes == highPlus.partialValue / 100,
              model.reservationBytes == model.projectedHighBytes else {
            throw FightboxRouteManifestError.invalid("calibrated model formula or band is invalid for \(cellId)")
        }
    }

    private static func isStableId(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 128 && value.utf8.allSatisfy { byte in
            (97...122).contains(byte) || (48...57).contains(byte)
                || byte == 45 || byte == 95 || byte == 46 || byte == 47
        }
    }

    private static func isLowerHex(_ value: String) -> Bool {
        value.utf8.allSatisfy { byte in
            (48...57).contains(byte) || (97...102).contains(byte)
        }
    }

    private static func manhattan(_ left: FightboxRouteGridIndex, _ right: FightboxRouteGridIndex) -> Int64 {
        abs(Int64(left.east) - Int64(right.east)) + abs(Int64(left.north) - Int64(right.north))
    }

    private static func intersection(
        _ left: FightboxRouteBoundsMm,
        _ right: FightboxRouteBoundsMm
    ) -> FightboxRouteBoundsMm? {
        let min = [Swift.max(left.min[0], right.min[0]), Swift.max(left.min[1], right.min[1])]
        let max = [Swift.min(left.max[0], right.max[0]), Swift.min(left.max[1], right.max[1])]
        guard min[0] < max[0], min[1] < max[1] else { return nil }
        return FightboxRouteBoundsMm(min: min, max: max)
    }

    private static func union(_ bounds: [FightboxRouteBoundsMm]) -> FightboxRouteBoundsMm {
        FightboxRouteBoundsMm(
            min: [bounds.map { $0.min[0] }.min()!, bounds.map { $0.min[1] }.min()!],
            max: [bounds.map { $0.max[0] }.max()!, bounds.map { $0.max[1] }.max()!]
        )
    }

    private func validateSha256(_ value: String, _ label: String) throws {
        guard value.utf8.count == 64,
              value.utf8.allSatisfy({ byte in
                  (48...57).contains(byte) || (97...102).contains(byte)
              }) else {
            throw FightboxRouteManifestError.invalid("\(label) SHA-256 is not lowercase hexadecimal")
        }
    }
}

public enum FightboxAuthoredRouteDirection: Sendable {
    case forward
    case reverse
}

/// Immutable control-plane route index. It never crosses into the audio callback
/// and returns at most one neighbor for the existing one-prepare coordinator.
public struct FightboxRouteNeighborSelector: Sendable {
    public let manifest: FightboxCityRouteManifest
    private let cellsById: [String: FightboxRouteCell]
    private let incidentByCellId: [String: [FightboxRouteAdjacency]]

    public init(manifest: FightboxCityRouteManifest) throws {
        try manifest.validate()
        self.manifest = manifest
        self.cellsById = Dictionary(uniqueKeysWithValues: manifest.cells.map { ($0.cellId, $0) })
        var incident: [String: [FightboxRouteAdjacency]] = [:]
        for adjacency in manifest.adjacencies {
            for cellId in adjacency.cells {
                incident[cellId, default: []].append(adjacency)
            }
        }
        self.incidentByCellId = incident.mapValues { rows in
            rows.sorted { $0.id < $1.id }
        }
    }

    public func cell(id: String) -> FightboxRouteCell? {
        cellsById[id]
    }

    /// Implements the frozen `min_inclusive_max_exclusive` ownership rule.
    public func ownerCell(eastMm: Int64, northMm: Int64) throws -> FightboxRouteCell? {
        let owners = manifest.cells.filter {
            $0.selection.ownershipBoundsCityEnuMm.containsHalfOpen(
                eastMm: eastMm,
                northMm: northMm
            )
        }
        guard owners.count <= 1 else {
            throw FightboxRouteManifestError.invalid("route ownership bounds overlap")
        }
        return owners.first
    }

    public func authoredNeighbor(
        activeCellId: String,
        direction: FightboxAuthoredRouteDirection
    ) throws -> FightboxRouteCell? {
        guard let active = cellsById[activeCellId] else {
            throw FightboxRouteManifestError.invalid("unknown active route cell \(activeCellId)")
        }
        let id = switch direction {
        case .forward: active.prefetch.forwardCellId
        case .reverse: active.prefetch.reverseCellId
        }
        guard let id else { return nil }
        guard let neighbor = cellsById[id] else {
            throw FightboxRouteManifestError.invalid("active route cell has a dangling authored neighbor")
        }
        return neighbor
    }

    /// Selects one incident physical neighbor from the velocity projection. Raw
    /// manifest route offsets are absolute and are intentionally never treated
    /// as signed displacement from the listener.
    public func directionalNeighbor(
        activeCellId: String,
        velocityEastMmPerSecond: Double,
        velocityNorthMmPerSecond: Double
    ) throws -> FightboxRouteCell? {
        guard let active = cellsById[activeCellId] else {
            throw FightboxRouteManifestError.invalid("unknown active route cell \(activeCellId)")
        }
        guard velocityEastMmPerSecond.isFinite, velocityNorthMmPerSecond.isFinite else {
            throw FightboxRouteManifestError.invalid("route velocity must be finite")
        }
        let speed = hypot(velocityEastMmPerSecond, velocityNorthMmPerSecond)
        guard speed > 0 else { return nil }
        var candidates: [(score: Double, id: String, cell: FightboxRouteCell)] = []
        for adjacency in incidentByCellId[activeCellId] ?? [] {
            guard let neighborId = adjacency.cells.first(where: { $0 != activeCellId }),
                  let neighbor = cellsById[neighborId] else { continue }
            let east = (neighbor.localToCityEnuM[0] - active.localToCityEnuM[0]) * 1_000.0
            let north = (neighbor.localToCityEnuM[1] - active.localToCityEnuM[1]) * 1_000.0
            let length = hypot(east, north)
            guard length > 0 else { continue }
            let score = (east * velocityEastMmPerSecond + north * velocityNorthMmPerSecond)
                / (length * speed)
            if score > 0 {
                candidates.append((score, neighborId, neighbor))
            }
        }
        return candidates.max { left, right in
            if left.score == right.score { return left.id > right.id }
            return left.score < right.score
        }?.cell
    }
}

private enum FightboxRouteStrictShape {
    static func validate(_ raw: Any) throws {
        let root = try object(raw, path: "$", keys: [
            "schema_version", "assembler_revision", "city_id", "route_id", "route_order",
            "grid_policy", "cells", "adjacencies", "owner_home", "four_cell_fixture",
            "installed_totals",
        ], optionalKeys: ["production_eligible"])
        try object(root["grid_policy"] as Any, path: "$.grid_policy", keys: [
            "probe_footprint_m", "stride_m", "pairwise_overlap_m", "ownership_guard_m",
            "geometry_halo_m", "maximum_resident_worlds", "maximum_prepared_neighbors",
            "maximum_resident_echo_authority_cells", "echo_authority_resident_cap_bytes",
        ])
        for (index, value) in try array(root["cells"] as Any, path: "$.cells").enumerated() {
            let path = "$.cells[\(index)]"
            let cell = try object(value, path: path, keys: [
                "sequence", "city_id", "cell_id", "grid_index", "local_to_city_enu_m",
                "probe_footprint_bounds_city_enu_mm", "geometry_halo_bounds_city_enu_mm",
                "world", "city_bake", "echo_authority", "selection", "prefetch", "installed_size",
            ])
            try grid(cell["grid_index"] as Any, path: "\(path).grid_index")
            try bounds(cell["probe_footprint_bounds_city_enu_mm"] as Any, path: "\(path).probe_footprint_bounds_city_enu_mm")
            try bounds(cell["geometry_halo_bounds_city_enu_mm"] as Any, path: "\(path).geometry_halo_bounds_city_enu_mm")
            try object(cell["world"] as Any, path: "\(path).world", keys: ["manifest_sha256", "mesh_sha256", "materials_sha256"])
            let bake = try object(cell["city_bake"] as Any, path: "\(path).city_bake", keys: [
                "state", "probe_plan_sidecar_sha256", "placement_policy_sha256",
                "probe_layout_sha256", "probe_count", "tier_ids", "maximum_layer_m",
                "above_maximum_layer", "completed",
            ], optionalKeys: ["calibrated_model"])
            if let model = bake["calibrated_model"], !(model is NSNull) {
                try object(model, path: "\(path).city_bake.calibrated_model", keys: [
                    "revision", "fixed_bytes", "owner_home_bytes_per_probe", "route_core_bytes_per_probe",
                    "transition_bytes_per_probe", "residual_bytes_per_probe", "open_owner_ground_ordered_pair_bytes",
                    "owner_home_probe_count", "route_core_probe_count", "transition_probe_count", "residual_probe_count",
                    "owner_ground_probe_count", "open_owner_ground_ordered_pair_count", "point_estimate_bytes",
                    "projected_low_bytes", "projected_high_bytes", "reservation_bytes", "mesh_observable",
                ])
                try object((model as! [String: Any])["mesh_observable"] as Any, path: "\(path).city_bake.calibrated_model.mesh_observable", keys: [
                    "algorithm", "coordinate_encoding", "owner_ground_height_mm", "owner_ground_probe_count",
                    "wall_edge_count", "blocked_owner_ground_ordered_pair_count", "open_owner_ground_ordered_pair_count",
                ], optionalKeys: ["mesh_sha256"])
            }
            if let completed = bake["completed"], !(completed is NSNull) {
                try object(
                    completed,
                    path: "\(path).city_bake.completed",
                    keys: [
                        "sidecar_sha256", "probe_batch_sha256", "probe_batch_size_bytes",
                        "path_data_size_bytes",
                    ],
                    optionalKeys: [
                        "probe_byte_estimate_v2_sha256", "probe_byte_estimate_v2_size_bytes",
                    ]
                )
            }
            if let echo = cell["echo_authority"], !(echo is NSNull) {
                try object(echo, path: "\(path).echo_authority", keys: [
                    "capability", "package_sidecar_path", "content_sha256", "serialized_size_bytes",
                    "resident_size_bytes", "anchor_set_sha256", "listener_layout_sha256",
                    "coordinate_frame_key", "static_anchor_count", "residency_scope",
                ])
            }
            let selection = try object(cell["selection"] as Any, path: "\(path).selection", keys: [
                "route_offset_mm", "ownership_bounds_city_enu_mm", "guard_bounds_city_enu_mm",
            ])
            try bounds(selection["ownership_bounds_city_enu_mm"] as Any, path: "\(path).selection.ownership_bounds_city_enu_mm")
            try bounds(selection["guard_bounds_city_enu_mm"] as Any, path: "\(path).selection.guard_bounds_city_enu_mm")
            try object(cell["prefetch"] as Any, path: "\(path).prefetch", keys: [
                "reverse_cell_id", "forward_cell_id", "raw_cell_bytes",
                "prepared_resident_estimate_bytes", "preparation_scratch_bytes",
                "echo_authority_resident_bytes", "echo_authority_load_policy", "estimate_revision",
            ])
            try object(cell["installed_size"] as Any, path: "\(path).installed_size", keys: [
                "package_bytes", "baked_artifact_bytes", "actual_installed_bytes",
                "projected_remaining_bake_bytes", "projected_complete_installed_bytes",
            ])
        }
        for (index, value) in try array(root["adjacencies"] as Any, path: "$.adjacencies").enumerated() {
            let path = "$.adjacencies[\(index)]"
            let row = try object(value, path: path, keys: [
                "id", "cells", "axis", "overlap_bounds_city_enu_mm",
                "ownership_switch_coordinate_mm", "overlap_probe_count", "overlap_probes_sha256",
            ])
            try bounds(row["overlap_bounds_city_enu_mm"] as Any, path: "\(path).overlap_bounds_city_enu_mm")
        }
        if let owner = root["owner_home"], !(owner is NSNull) {
            try object(owner, path: "$.owner_home", keys: ["cell_id", "tier_id", "probe_count"])
        }
        if let fixture = root["four_cell_fixture"], !(fixture is NSNull) {
            let row = try object(fixture, path: "$.four_cell_fixture", keys: [
                "state", "bakes_launched", "grid_min", "grid_max",
                "streamed_union_bounds_city_enu_mm", "monolithic_oracle_bounds_city_enu_mm",
                "monolithic_oracle_path_range_m", "streamed_cell_bakes_required",
                "monolithic_oracle_bakes_required", "seam_ids",
            ])
            try grid(row["grid_min"] as Any, path: "$.four_cell_fixture.grid_min")
            try grid(row["grid_max"] as Any, path: "$.four_cell_fixture.grid_max")
            try bounds(row["streamed_union_bounds_city_enu_mm"] as Any, path: "$.four_cell_fixture.streamed_union_bounds_city_enu_mm")
            try bounds(row["monolithic_oracle_bounds_city_enu_mm"] as Any, path: "$.four_cell_fixture.monolithic_oracle_bounds_city_enu_mm")
        }
        try object(root["installed_totals"] as Any, path: "$.installed_totals", keys: [
            "actual_installed_bytes", "projected_remaining_bake_bytes",
            "projected_complete_installed_bytes", "completed_cell_count", "planned_cell_count",
        ])
    }

    @discardableResult
    private static func object(
        _ raw: Any,
        path: String,
        keys: Set<String>,
        optionalKeys: Set<String> = []
    ) throws -> [String: Any] {
        guard let value = raw as? [String: Any] else {
            throw FightboxRouteManifestError.invalid("\(path) must be an object")
        }
        let actual = Set(value.keys)
        let allowed = keys.union(optionalKeys)
        guard keys.isSubset(of: actual), actual.isSubset(of: allowed) else {
            let unknown = actual.subtracting(allowed).sorted()
            let missing = keys.subtracting(actual).sorted()
            throw FightboxRouteManifestError.invalid("\(path) keys mismatch; unknown=\(unknown), missing=\(missing)")
        }
        return value
    }

    private static func array(_ raw: Any, path: String) throws -> [Any] {
        guard let value = raw as? [Any] else {
            throw FightboxRouteManifestError.invalid("\(path) must be an array")
        }
        return value
    }

    private static func grid(_ raw: Any, path: String) throws {
        try object(raw, path: path, keys: ["east", "north"])
    }

    private static func bounds(_ raw: Any, path: String) throws {
        try object(raw, path: path, keys: ["min", "max"])
    }
}
