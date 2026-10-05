import CoreFoundation
import CryptoKit
import Foundation

public struct FightboxRouteCellArtifactLocation: Sendable, Equatable {
    public let packageDirectory: URL
    public let bakeDirectory: URL

    public init(packageDirectory: URL, bakeDirectory: URL) {
        self.packageDirectory = packageDirectory
        self.bakeDirectory = bakeDirectory
    }
}

public struct FightboxVerifiedRouteCellArtifacts: Sendable, Equatable {
    public let cityId: String
    public let cellId: String
    public let packageDirectory: URL
    public let bakeDirectory: URL
    public let rawCellBytes: UInt64
    public let preparedResidentBytes: UInt64
    public let preparationScratchBytes: UInt64
}

/// Control-plane artifact verifier. A descriptor is exposed only after the
/// immutable route identity, package payloads, and completed bake all agree.
@available(macOS 10.15, *)
public struct FightboxRouteArtifactResolver: Sendable {
    public let selector: FightboxRouteNeighborSelector
    private let locationsByCellId: [String: FightboxRouteCellArtifactLocation]

    public init(
        selector: FightboxRouteNeighborSelector,
        locationsByCellId: [String: FightboxRouteCellArtifactLocation],
        allowEvidenceOnly: Bool = false
    ) throws {
        guard selector.manifest.productionEligible == true || allowEvidenceOnly else {
            throw FightboxRouteManifestError.invalid(
                "frozen evidence-only route cannot construct a production artifact resolver"
            )
        }
        let expected = Set(selector.manifest.cells.map(\.cellId))
        guard Set(locationsByCellId.keys) == expected else {
            throw FightboxRouteManifestError.invalid("artifact resolver locations must exactly cover the route")
        }
        self.selector = selector
        self.locationsByCellId = locationsByCellId
    }

    public func verify(cellId: String) throws -> FightboxVerifiedRouteCellArtifacts {
        guard let cell = selector.cell(id: cellId), let location = locationsByCellId[cellId] else {
            throw FightboxRouteManifestError.invalid("unknown route artifact cell \(cellId)")
        }
        guard cell.cityBake.state == .bakedProbeBatch, let completed = cell.cityBake.completed else {
            throw FightboxRouteManifestError.invalid("route cell \(cellId) is not a completed streamed bake")
        }
        let package = location.packageDirectory.standardizedFileURL
        let bake = location.bakeDirectory.standardizedFileURL
        let manifestURL = package.appendingPathComponent("manifest.json", isDirectory: false)
        let meshURL = package.appendingPathComponent("mesh.bin", isDirectory: false)
        let materialsURL = package.appendingPathComponent("materials.json", isDirectory: false)
        let planURL = package.appendingPathComponent("capabilities/city-bake-v2.json", isDirectory: false)
        let batchURL = bake.appendingPathComponent("probe-batch.bin", isDirectory: false)
        let completedURL = bake.appendingPathComponent("capabilities/city-bake-v2.json", isDirectory: false)
        let metadataURL = bake.appendingPathComponent("probe-batch-metadata.json", isDirectory: false)
        let estimateURL = bake.appendingPathComponent("probe-byte-estimate-v2.json", isDirectory: false)

        var expectedPackageFiles: Set<String> = [
            "manifest.json", "mesh.bin", "materials.json", "capabilities/city-bake-v2.json",
        ]
        if let echo = cell.echoAuthority {
            expectedPackageFiles.insert(echo.packageSidecarPath)
        }
        var expectedBakeFiles: Set<String> = [
            "probe-batch.bin", "probe-batch-metadata.json", "capabilities/city-bake-v2.json",
        ]
        if completed.probeByteEstimateV2Sha256 != nil {
            expectedBakeFiles.insert("probe-byte-estimate-v2.json")
        }
        try requireExactRegularFileClosure(
            package, expected: expectedPackageFiles, label: "world package"
        )
        try requireExactRegularFileClosure(
            bake, expected: expectedBakeFiles, label: "completed bake"
        )

        let manifestData = try loadRegularFile(manifestURL, label: "world manifest")
        try requireDigest(manifestData, expected: cell.world.manifestSha256, label: "world manifest")
        let manifest = try jsonObject(manifestData, label: "world manifest")
        guard manifest["schema_version"] as? String == "fightbox.world-manifest.v2",
              integer(manifest["format_version"]) == 2,
              let world = manifest["world"] as? [String: Any],
              let city = world["city"] as? [String: Any],
              city["id"] as? String == cell.cityId,
              let packageCell = world["cell"] as? [String: Any],
              packageCell["id"] as? String == cell.cellId,
              let index = packageCell["grid_index"] as? [String: Any],
              integer(index["east"]) == Int64(cell.gridIndex.east),
              integer(index["north"]) == Int64(cell.gridIndex.north),
              doubleArray(packageCell["local_to_city_enu_m"], count: 3) == cell.localToCityEnuM,
              packageBoundsAreWithinRoute(packageCell, cell: cell),
              let ranges = packageCell["supported_ranges"] as? [String: Any],
              double(ranges["baked_path_horizon_m"]) == 600,
              double(ranges["geometry_halo_m"]) == 600,
              double(ranges["probe_footprint_m"]) == 585,
              let policy = world["mobile_cell_policy"] as? [String: Any],
              integer(policy["hard_raw_probe_payload_bytes"]) == 67_108_864,
              integer(policy["maximum_prepared_neighbors"]) == 1,
              integer(policy["maximum_resident_worlds"]) == 2,
              double(policy["ownership_guard_m"]) == 50,
              double(policy["pairwise_overlap_m"]) == 100,
              double(policy["stride_m"]) == 485,
              integer(policy["target_raw_probe_payload_bytes"]) == 50_331_648,
              manifest["materials_content_sha256"] as? String == cell.world.materialsSha256,
              let mesh = manifest["mesh"] as? [String: Any],
              mesh["content_sha256"] as? String == cell.world.meshSha256 else {
            throw FightboxRouteManifestError.invalid("world package identity disagrees with route cell \(cellId)")
        }

        let meshData = try loadRegularFile(meshURL, label: "mesh payload")
        try requireDigest(
            meshData,
            expected: cell.world.meshSha256,
            label: "mesh payload"
        )
        try requireDigest(
            try loadRegularFile(materialsURL, label: "materials payload"),
            expected: cell.world.materialsSha256,
            label: "materials payload"
        )
        let planData = try loadRegularFile(planURL, label: "city bake plan")
        try requireDigest(
            planData,
            expected: cell.cityBake.probePlanSidecarSha256,
            label: "city bake plan"
        )
        let planAuthority = try jsonObject(planData, label: "city bake plan")
        guard planAuthority["schema_version"] as? String == "fightbox.city-bake.v2",
              planAuthority["artifact_state"] as? String == "probe_plan",
              let byteEstimate = planAuthority["byte_estimate"] as? [String: Any],
              integer(byteEstimate["path_horizon_m"]) == 600,
              integer(byteEstimate["probe_count"]) == Int64(cell.cityBake.probeCount),
              integer(byteEstimate["target_raw_probe_payload_bytes"]) == 50_331_648,
              integer(byteEstimate["hard_raw_probe_payload_bytes"]) == 67_108_864,
              byteEstimate["projected_high_within_hard_limit"] as? Bool == true,
              let projectedHigh = integer(byteEstimate["projected_high_bytes"]),
              projectedHigh <= 67_108_864 else {
            throw FightboxRouteManifestError.invalid(
                "city bake plan is not mobile-admissible for route cell \(cellId)"
            )
        }
        if let echo = cell.echoAuthority {
            let echoURL = package.appendingPathComponent(echo.packageSidecarPath, isDirectory: false)
            let echoData = try loadRegularFile(echoURL, label: "echo authority")
            guard UInt64(echoData.count) == echo.serializedSizeBytes else {
                throw FightboxRouteManifestError.invalid(
                    "echo authority size disagrees with route cell \(cellId)"
                )
            }
            try requireDigest(
                echoData,
                expected: echo.contentSha256,
                label: "echo authority"
            )
        }

        let completedData = try loadRegularFile(completedURL, label: "completed city bake sidecar")
        try requireDigest(completedData, expected: completed.sidecarSha256, label: "completed city bake sidecar")
        let authority = try jsonObject(completedData, label: "completed city bake sidecar")
        guard authority["schema_version"] as? String == "fightbox.city-bake.v2",
              authority["artifact_state"] as? String == "baked_probe_batch",
              authority["city_id"] as? String == cell.cityId,
              authority["cell_id"] as? String == cell.cellId,
              authority["probe_plan_content_sha256"] as? String == cell.cityBake.probePlanSidecarSha256,
              authority["placement_policy_sha256"] as? String == cell.cityBake.placementPolicySha256,
              authority["probe_layout_sha256"] as? String == cell.cityBake.probeLayoutSha256,
              authority["package_mesh_sha256"] as? String == cell.world.meshSha256,
              authority["package_materials_sha256"] as? String == cell.world.materialsSha256,
              integer(authority["planned_probe_count"]) == Int64(cell.cityBake.probeCount),
              let authoritativeBatch = authority["probe_batch"] as? [String: Any],
              integer(authoritativeBatch["probe_count"]) == Int64(cell.cityBake.probeCount),
              integer(authoritativeBatch["serialized_size_bytes"]) == Int64(completed.probeBatchSizeBytes),
              integer(authoritativeBatch["path_data_size_bytes"]) == Int64(completed.pathDataSizeBytes),
              authoritativeBatch["serialized_sha256"] as? String == completed.probeBatchSha256,
              let pathing = authority["pathing"] as? [String: Any],
              integer(pathing["path_range_m"]) == 600,
              let telemetry = authority["telemetry"] as? [String: Any],
              integer(telemetry["final_bake_progress_millionths"]) == 1_000_000 else {
            throw FightboxRouteManifestError.invalid("completed city bake authority disagrees with route cell \(cellId)")
        }

        var estimateAuthority: [String: Any]?
        var estimateRevision: String?
        switch (completed.probeByteEstimateV2Sha256, completed.probeByteEstimateV2SizeBytes) {
        case (.none, .none):
            break // Frozen pre-envelope route evidence is readable but cannot bind a new envelope.
        case (.some(let expectedDigest), .some(let expectedSize)):
            let estimateData = try loadRegularFile(estimateURL, label: "probe-byte estimate v2")
            guard UInt64(estimateData.count) == expectedSize else {
                throw FightboxRouteManifestError.invalid("probe-byte estimate size disagrees with route cell \(cellId)")
            }
            try requireDigest(estimateData, expected: expectedDigest, label: "probe-byte estimate v2")
            let estimate = try jsonObject(estimateData, label: "probe-byte estimate v2")
            guard estimate["schema_version"] as? String == "fightbox.probe-byte-estimate.v2",
                  estimate["artifact_state"] as? String == "completed",
                  let subject = estimate["subject"] as? [String: Any],
                  subject["schema_version"] as? String == "fightbox.city-bake.v2",
                  subject["manifest_path"] as? String == "capabilities/city-bake-v2.json",
                  subject["manifest_sha256"] as? String == completed.sidecarSha256,
                  subject["package_manifest_sha256"] as? String == cell.world.manifestSha256,
                  let request = estimate["request"] as? [String: Any],
                  request["probe_plan_sha256"] as? String == cell.cityBake.probePlanSidecarSha256,
                  request["placement_policy_sha256"] as? String == cell.cityBake.placementPolicySha256,
                  request["probe_layout_sha256"] as? String == cell.cityBake.probeLayoutSha256,
                  integer(request["probe_count"]) == Int64(cell.cityBake.probeCount),
                  integer(request["path_horizon_m"]) == 600,
                  let model = estimate["model"] as? [String: Any],
                  integer(model["projected_high_bytes"]) ?? Int64.max <= 67_108_864,
                  let observed = estimate["observed"] as? [String: Any],
                  integer(observed["probe_count"]) == Int64(cell.cityBake.probeCount),
                  integer(observed["serialized_size_bytes"]) == Int64(completed.probeBatchSizeBytes),
                  observed["payload_sha256"] as? String == completed.probeBatchSha256,
                  integer(observed["artifact_bytes"]) == Int64(cell.installedSize.bakedArtifactBytes) else {
                throw FightboxRouteManifestError.invalid("probe-byte estimate authority disagrees with route cell \(cellId)")
            }
            estimateAuthority = estimate
            estimateRevision = (estimate["model"] as? [String: Any])?["revision"] as? String
        default:
            throw FightboxRouteManifestError.invalid("route cell \(cellId) has a partial probe-byte estimate identity")
        }

        let completedRevision = authority["estimator_revision"] as? String
        let legacyEstimatorRevision = "reachable-ordered-pairs-p256-q10-fixed64k-v1"
        let fixedEstimatorRevision = "wave17-fixed-tier-mesh-open-pairs-v2"
        guard completedRevision == nil || completedRevision == legacyEstimatorRevision || completedRevision == fixedEstimatorRevision else {
            throw FightboxRouteManifestError.invalid("completed city bake has an unknown estimator revision")
        }
        if let estimateRevision, estimateRevision != legacyEstimatorRevision && estimateRevision != fixedEstimatorRevision {
            throw FightboxRouteManifestError.invalid("probe-byte estimate has an unknown estimator revision")
        }
        guard completedRevision == nil || estimateRevision == nil || completedRevision == estimateRevision else {
            throw FightboxRouteManifestError.invalid("completed bake and probe-byte estimate estimator revisions disagree")
        }
        if completedRevision == fixedEstimatorRevision || estimateRevision == fixedEstimatorRevision {
            guard completedRevision == fixedEstimatorRevision, estimateRevision == fixedEstimatorRevision,
                  let estimateAuthority else {
                throw FightboxRouteManifestError.invalid("fixed estimator requires matching completed and estimate authorities")
            }
            try verifyFixedEstimator(
                cell: cell,
                completed: completed,
                planAuthority: planAuthority,
                authority: authority,
                estimate: estimateAuthority,
                meshData: meshData
            )
        } else {
            // The pre-Wave-17 Q×10 envelope remains intentionally readable.  It
            // must not acquire additive calibrated fields by accident.
            if let estimateAuthority,
               let model = estimateAuthority["model"] as? [String: Any],
               model["calibrated_model"] != nil {
                throw FightboxRouteManifestError.invalid("legacy estimator must not carry calibrated model fields")
            }
            if authority["calibrated_model"] != nil {
                throw FightboxRouteManifestError.invalid("legacy completed bake must not carry calibrated model fields")
            }
        }

        let metadataData = try loadRegularFile(metadataURL, label: "probe batch metadata")
        let metadata = try jsonObject(metadataData, label: "probe batch metadata")
        guard metadata["schema_version"] as? String == "fightbox.steam-audio.probe-batch.v1",
              integer(metadata["probe_count"]) == Int64(cell.cityBake.probeCount),
              integer(metadata["serialized_size_bytes"]) == Int64(completed.probeBatchSizeBytes),
              integer(metadata["path_data_size_bytes"]) == Int64(completed.pathDataSizeBytes),
              metadata["content_sha256"] as? String == completed.probeBatchSha256,
              integer(metadata["final_bake_progress_millionths"]) == 1_000_000 else {
            throw FightboxRouteManifestError.invalid("probe batch metadata disagrees with route cell \(cellId)")
        }
        let batchData = try loadRegularFile(batchURL, label: "probe batch")
        guard UInt64(batchData.count) == completed.probeBatchSizeBytes else {
            throw FightboxRouteManifestError.invalid("probe batch size disagrees with route cell \(cellId)")
        }
        try requireDigest(batchData, expected: completed.probeBatchSha256, label: "probe batch")

        return FightboxVerifiedRouteCellArtifacts(
            cityId: cell.cityId,
            cellId: cell.cellId,
            packageDirectory: package,
            bakeDirectory: bake,
            rawCellBytes: cell.prefetch.rawCellBytes,
            preparedResidentBytes: cell.prefetch.preparedResidentEstimateBytes,
            preparationScratchBytes: cell.prefetch.preparationScratchBytes
        )
    }

    // MARK: - Strict Wave 17 fixed estimator

    private struct FixedPoint: Hashable {
        let east: Int64
        let north: Int64
    }

    private struct FixedEdge: Hashable {
        let left: FixedPoint
        let right: FixedPoint

        init(_ first: FixedPoint, _ second: FixedPoint) {
            if first.east < second.east ||
                (first.east == second.east && first.north <= second.north) {
                left = first
                right = second
            } else {
                left = second
                right = first
            }
        }
    }

    private struct FixedTierFact: Equatable {
        let id: String
        let groundSpacingM: Int64
        let analysisSpacingM: Int64
        let layers: [(id: String, upMm: Int64, spacingM: Int64, probeCount: UInt64)]
        let probeCount: UInt64

        static func == (lhs: FixedTierFact, rhs: FixedTierFact) -> Bool {
            guard lhs.id == rhs.id,
                  lhs.groundSpacingM == rhs.groundSpacingM,
                  lhs.analysisSpacingM == rhs.analysisSpacingM,
                  lhs.probeCount == rhs.probeCount,
                  lhs.layers.count == rhs.layers.count else { return false }
            return zip(lhs.layers, rhs.layers).allSatisfy {
                $0.id == $1.id && $0.upMm == $1.upMm &&
                    $0.spacingM == $1.spacingM && $0.probeCount == $1.probeCount
            }
        }
    }

    private struct FixedPlanFacts {
        let tiers: [FixedTierFact]
        let ownerHeightMm: Int64
        let ownerGroundProbes: [FixedPoint]
    }

    private struct QuantizedMesh {
        let vertices: [(east: Int64, north: Int64, up: Int64)]
        let triangles: [(UInt32, UInt32, UInt32)]
    }

    private func verifyFixedEstimator(
        cell: FightboxRouteCell,
        completed: FightboxRouteCompletedBakeBinding,
        planAuthority: [String: Any],
        authority: [String: Any],
        estimate: [String: Any],
        meshData: Data
    ) throws {
        let fixedRevision = "wave17-fixed-tier-mesh-open-pairs-v2"
        let placementSHA = "b798fc0eb39a9f8da5c14595b0452dc38e798bc35c5da7fc95035da09f2fca0a"
        let hardLimit: UInt64 = 67_108_864

        // These are the Rust deny_unknown_fields envelopes.  Apply the
        // additive shape checks only to the new revision so old Q×10 JSON
        // remains byte-compatible and readable.
        try requireExactKeys(estimate, [
            "schema_version", "artifact_state", "subject", "request", "model", "observed",
        ], label: "fixed probe-byte estimate")
        let subject = try requiredObject(estimate, "subject", label: "fixed estimate subject")
        try requireExactKeys(subject, ["schema_version", "manifest_path", "manifest_sha256", "package_manifest_sha256"], label: "fixed estimate subject")
        guard subject["schema_version"] as? String == "fightbox.city-bake.v2",
              subject["manifest_path"] as? String == "capabilities/city-bake-v2.json",
              subject["manifest_sha256"] as? String == completed.sidecarSha256,
              subject["package_manifest_sha256"] as? String == cell.world.manifestSha256,
              isSHA256(subject["manifest_sha256"]), isSHA256(subject["package_manifest_sha256"]) else {
            throw invalidFixed("fixed estimate subject is not bound to the completed/package authorities")
        }

        let request = try requiredObject(estimate, "request", label: "fixed estimate request")
        try requireExactKeys(request, [
            "request_sha256", "mesh_sha256", "materials_sha256", "probe_plan_sha256",
            "placement_policy_sha256", "probe_layout_sha256", "probe_count", "path_horizon_m",
            "pathing", "sdk",
        ], label: "fixed estimate request")
        guard request["request_sha256"] as? String != nil,
              isSHA256(request["request_sha256"]),
              request["mesh_sha256"] as? String == cell.world.meshSha256,
              request["materials_sha256"] as? String == cell.world.materialsSha256,
              request["probe_plan_sha256"] as? String == cell.cityBake.probePlanSidecarSha256,
              request["placement_policy_sha256"] as? String == placementSHA,
              request["probe_layout_sha256"] as? String == cell.cityBake.probeLayoutSha256,
              integer(request["probe_count"]) == Int64(cell.cityBake.probeCount),
              integer(request["path_horizon_m"]) == 600,
              isSHA256(request["mesh_sha256"]), isSHA256(request["materials_sha256"]),
              isSHA256(request["probe_plan_sha256"]), isSHA256(request["placement_policy_sha256"]),
              isSHA256(request["probe_layout_sha256"]) else {
            throw invalidFixed("fixed estimate request is not bound to the route")
        }
        let pathing = try requiredObject(request, "pathing", label: "fixed estimate pathing")
        try requireExactKeys(pathing, [
            "visibility_range_m", "visibility_samples", "visibility_threshold",
            "probe_visibility_radius_m", "threads",
        ], label: "fixed estimate pathing")
        guard let visibilityRange = double(pathing["visibility_range_m"]), visibilityRange > 0,
              integer(pathing["visibility_samples"]) ?? 0 > 0,
              let threshold = double(pathing["visibility_threshold"]), threshold >= 0, threshold <= 1,
              let visibilityRadius = double(pathing["probe_visibility_radius_m"]), visibilityRadius >= 0,
              integer(pathing["threads"]) ?? 0 > 0 else {
            throw invalidFixed("fixed estimate pathing is invalid")
        }
        let sdk = try requiredObject(request, "sdk", label: "fixed estimate SDK")
        try requireExactKeys(sdk, ["metadata_schema", "steam_audio_version", "upstream_commit", "baker_revision"], label: "fixed estimate SDK")
        guard sdk["metadata_schema"] as? String == "fightbox.steam-audio.probe-batch.v1",
              sdk["steam_audio_version"] as? String == "4.8.1",
              sdk["upstream_commit"] as? String == "0da1825",
              sdk["baker_revision"] as? String == "steam-audio-explicit-probes-v1" else {
            throw invalidFixed("fixed estimate SDK identity is invalid")
        }
        guard let requestSHA = canonicalFixedRequestSHA(request),
              requestSHA == request["request_sha256"] as? String else {
            throw invalidFixed("fixed estimate request_sha256 does not bind the canonical request")
        }

        let model = try requiredObject(estimate, "model", label: "fixed estimate model")
        try requireExactKeys(model, [
            "revision", "probe_bytes", "pair_bytes", "fixed_bytes", "pair_count", "pair_count_kind",
            "estimated_raw_bytes", "projected_low_bytes", "projected_high_bytes", "reservation_bytes",
            "calibrated_model",
        ], label: "fixed estimate model")
        guard model["revision"] as? String == fixedRevision,
              integer(model["probe_bytes"]) == 0,
              integer(model["pair_bytes"]) == 0,
              integer(model["fixed_bytes"]) == 0,
              integer(model["pair_count"]) == 0,
              model["pair_count_kind"] as? String == "exact",
              model["calibrated_model"] != nil else {
            throw invalidFixed("fixed estimate carries legacy or mixed model fields")
        }

        try requireExactKeys(planAuthority, [
            "schema_version", "artifact_state", "coordinate_frame", "coordinate_encoding", "placement_rule",
            "placement_policy_sha256", "probe_layout_sha256", "cell", "policy", "tier_summaries", "grid_set",
            "probes", "byte_estimate",
        ], label: "fixed probe plan")
        let fixedCell = try requiredObject(planAuthority, "cell", label: "fixed probe plan cell")
        try requireExactKeys(fixedCell, [
            "grid_index", "local_to_city_enu_mm", "probe_footprint_m", "stride_m", "pairwise_overlap_m",
            "ownership_guard_m", "geometry_material_halo_m", "baked_path_horizon_m",
            "probe_footprint_bounds_city_enu_mm", "ownership_bounds_city_enu_mm",
            "geometry_material_bounds_city_enu_mm", "probe_bounds_rule", "ownership_bounds_rule",
        ], label: "fixed probe plan cell")
        let fixedByteEstimate = try requiredObject(planAuthority, "byte_estimate", label: "fixed probe plan byte estimate")
        try requireExactKeys(fixedByteEstimate, [
            "estimator_revision", "path_horizon_m", "probe_count", "reachable_ordered_pair_count",
            "estimated_raw_bytes", "projected_low_bytes", "projected_high_bytes", "target_raw_probe_payload_bytes",
            "hard_raw_probe_payload_bytes", "estimate_within_target", "projected_high_within_hard_limit",
        ], label: "fixed probe plan byte estimate")
        guard planAuthority["placement_policy_sha256"] as? String == placementSHA,
              fixedByteEstimate["estimator_revision"] as? String == "reachable-ordered-pairs-p256-q10-fixed64k-v1",
              integer(fixedByteEstimate["path_horizon_m"]) == 600 else {
            throw invalidFixed("fixed probe plan is not bound to the calibrated placement or 600 m horizon")
        }
        let facts = try recomputeFixedPlanFacts(planAuthority, cell: cell)
        let mesh = try parseFixedMesh(meshData)
        let observable = try recomputeMeshObservable(
            mesh: mesh,
            facts: facts,
            localToCityMm: try planLocalTranslation(planAuthority, cell: cell)
        )
        let calibrated = try requiredObject(model, "calibrated_model", label: "fixed estimate calibrated model")
        try requireExactKeys(calibrated, [
            "revision", "fixed_bytes", "owner_home_bytes_per_probe", "route_core_bytes_per_probe",
            "transition_bytes_per_probe", "residual_bytes_per_probe", "open_owner_ground_ordered_pair_bytes",
            "tier_counts", "point_bytes", "projected_low_bytes", "projected_high_bytes", "reservation_bytes",
            "mesh_observable",
        ], label: "fixed estimate calibrated model")
        guard calibrated["revision"] as? String == fixedRevision,
              integer(calibrated["fixed_bytes"]) == 6_000,
              integer(calibrated["owner_home_bytes_per_probe"]) == 3_315,
              integer(calibrated["route_core_bytes_per_probe"]) == 31,
              integer(calibrated["transition_bytes_per_probe"]) == 54,
              integer(calibrated["residual_bytes_per_probe"]) == 30,
              integer(calibrated["open_owner_ground_ordered_pair_bytes"]) == 14 else {
            throw invalidFixed("fixed estimate coefficients are not canonical")
        }
        let tierCounts = try requiredObject(calibrated, "tier_counts", label: "fixed estimate tier counts")
        try requireExactKeys(tierCounts, ["owner_home_count", "route_core_count", "transition_count", "residual_count"], label: "fixed estimate tier counts")
        let expectedCounts = facts.tiers.map(\.probeCount)
        guard expectedCounts.count == 4,
              unsigned(tierCounts["owner_home_count"]) == expectedCounts[0],
              unsigned(tierCounts["route_core_count"]) == expectedCounts[1],
              unsigned(tierCounts["transition_count"]) == expectedCounts[2],
              unsigned(tierCounts["residual_count"]) == expectedCounts[3] else {
            throw invalidFixed("fixed estimate tier counts do not match the probe plan")
        }
        let estimateMesh = try requiredObject(calibrated, "mesh_observable", label: "fixed estimate mesh observable")
        try requireExactKeys(estimateMesh, [
            "algorithm", "coordinate_encoding", "mesh_sha256", "owner_ground_height_mm",
            "owner_ground_probe_count", "wall_edge_count", "blocked_owner_ground_ordered_pair_count",
            "open_owner_ground_ordered_pair_count",
        ], label: "fixed estimate mesh observable")
        try requireMeshObservable(estimateMesh, observable: observable, meshSHA: cell.world.meshSha256, includeMeshSHA: true)

        let point = try fixedPointBytes(
            owner: expectedCounts[0], routeCore: expectedCounts[1], transition: expectedCounts[2], residual: expectedCounts[3],
            openPairs: observable.openPairs
        )
        let low = try fixedBand(point, factor: 70, label: "lower")
        let high = try fixedBand(point, factor: 130, label: "upper")
        guard unsigned(calibrated["point_bytes"]) == point,
              unsigned(calibrated["projected_low_bytes"]) == low,
              unsigned(calibrated["projected_high_bytes"]) == high,
              unsigned(calibrated["reservation_bytes"]) == high,
              unsigned(model["estimated_raw_bytes"]) == point,
              unsigned(model["projected_low_bytes"]) == low,
              unsigned(model["projected_high_bytes"]) == high,
              unsigned(model["reservation_bytes"]) == high,
              high <= hardLimit else {
            throw invalidFixed("fixed estimate formula, band, reservation, or hard limit is invalid")
        }

        let observed = try requiredObject(estimate, "observed", label: "fixed estimate observed")
        try requireExactKeys(observed, ["probe_count", "path_data_size_bytes", "serialized_size_bytes", "payload_sha256", "artifact_bytes"], label: "fixed estimate observed")
        guard integer(observed["probe_count"]) == Int64(cell.cityBake.probeCount),
              let pathDataSize = unsigned(observed["path_data_size_bytes"]),
              pathDataSize <= completed.pathDataSizeBytes,
              integer(observed["serialized_size_bytes"]) == Int64(completed.probeBatchSizeBytes),
              observed["payload_sha256"] as? String == completed.probeBatchSha256,
              isSHA256(observed["payload_sha256"]),
              unsigned(observed["artifact_bytes"]) == cell.installedSize.bakedArtifactBytes,
              let observedBytes = unsigned(observed["artifact_bytes"]),
              observedBytes >= low, observedBytes <= high, observedBytes <= hardLimit,
              let serialized = unsigned(observed["serialized_size_bytes"]),
              serialized <= observedBytes, serialized <= high else {
            throw invalidFixed("fixed observed artifact is outside its inclusive calibrated band")
        }

        try verifyCompletedFixedModel(
            authority: authority,
            cell: cell,
            completed: completed,
            facts: facts,
            observable: observable,
            point: point,
            low: low,
            high: high,
            planAuthority: planAuthority
        )
    }

    private func verifyCompletedFixedModel(
        authority: [String: Any],
        cell: FightboxRouteCell,
        completed: FightboxRouteCompletedBakeBinding,
        facts: FixedPlanFacts,
        observable: (ownerHeightMm: Int64, ownerCount: UInt64, wallEdges: UInt64, blockedPairs: UInt64, openPairs: UInt64),
        point: UInt64,
        low: UInt64,
        high: UInt64,
        planAuthority: [String: Any]
    ) throws {
        let fixedRevision = "wave17-fixed-tier-mesh-open-pairs-v2"
        try requireExactKeys(authority, [
            "schema_version", "artifact_state", "coordinate_frame", "coordinate_encoding", "placement_rule",
            "probe_plan_content_sha256", "placement_policy_sha256", "probe_layout_sha256", "estimator_revision",
            "calibrated_model", "city_id", "cell_id", "cell_grid_index", "tier_summaries", "sky_pathing_policy",
            "planned_probe_count", "estimated_raw_bytes", "projected_low_bytes", "projected_high_bytes",
            "package_mesh_sha256", "package_materials_sha256", "pathing", "probe_batch", "telemetry",
        ], label: "fixed completed city-bake authority")
        guard authority["estimator_revision"] as? String == fixedRevision,
              authority["placement_policy_sha256"] as? String == "b798fc0eb39a9f8da5c14595b0452dc38e798bc35c5da7fc95035da09f2fca0a",
              authority["probe_plan_content_sha256"] as? String == cell.cityBake.probePlanSidecarSha256,
              authority["probe_layout_sha256"] as? String == cell.cityBake.probeLayoutSha256,
              authority["package_mesh_sha256"] as? String == cell.world.meshSha256,
              authority["package_materials_sha256"] as? String == cell.world.materialsSha256,
              integer(authority["planned_probe_count"]) == Int64(cell.cityBake.probeCount),
              unsigned(authority["estimated_raw_bytes"]) == point,
              unsigned(authority["projected_low_bytes"]) == low,
              unsigned(authority["projected_high_bytes"]) == high else {
            throw invalidFixed("fixed completed authority is not bound to the route/model")
        }
        let completedTiers = try parseTierFacts(authority["tier_summaries"], probes: nil, label: "fixed completed tier summaries")
        guard completedTiers == facts.tiers else {
            throw invalidFixed("completed tier summaries differ from the probe plan")
        }
        let calibrated = try requiredObject(authority, "calibrated_model", label: "fixed completed calibrated model")
        try requireExactKeys(calibrated, [
            "revision", "fixed_bytes", "owner_home_bytes_per_probe", "route_core_bytes_per_probe",
            "transition_bytes_per_probe", "residual_bytes_per_probe", "open_owner_ground_ordered_pair_bytes",
            "owner_home_probe_count", "route_core_probe_count", "transition_probe_count", "residual_probe_count",
            "owner_ground_probe_count", "open_owner_ground_ordered_pair_count", "point_estimate_bytes",
            "projected_low_bytes", "projected_high_bytes", "reservation_bytes", "mesh_observable",
        ], label: "fixed completed calibrated model")
        guard calibrated["revision"] as? String == fixedRevision,
              integer(calibrated["fixed_bytes"]) == 6_000,
              integer(calibrated["owner_home_bytes_per_probe"]) == 3_315,
              integer(calibrated["route_core_bytes_per_probe"]) == 31,
              integer(calibrated["transition_bytes_per_probe"]) == 54,
              integer(calibrated["residual_bytes_per_probe"]) == 30,
              integer(calibrated["open_owner_ground_ordered_pair_bytes"]) == 14,
              unsigned(calibrated["owner_home_probe_count"]) == facts.tiers[0].probeCount,
              unsigned(calibrated["route_core_probe_count"]) == facts.tiers[1].probeCount,
              unsigned(calibrated["transition_probe_count"]) == facts.tiers[2].probeCount,
              unsigned(calibrated["residual_probe_count"]) == facts.tiers[3].probeCount,
              unsigned(calibrated["owner_ground_probe_count"]) == observable.ownerCount,
              unsigned(calibrated["open_owner_ground_ordered_pair_count"]) == observable.openPairs,
              unsigned(calibrated["point_estimate_bytes"]) == point,
              unsigned(calibrated["projected_low_bytes"]) == low,
              unsigned(calibrated["projected_high_bytes"]) == high,
              unsigned(calibrated["reservation_bytes"]) == high else {
            throw invalidFixed("fixed completed calibrated coefficients/counts are invalid")
        }
        let meshObject = try requiredObject(calibrated, "mesh_observable", label: "fixed completed mesh observable")
        let completedMeshKeys: Set<String> = [
            "algorithm", "coordinate_encoding", "owner_ground_height_mm", "owner_ground_probe_count",
            "wall_edge_count", "blocked_owner_ground_ordered_pair_count", "open_owner_ground_ordered_pair_count",
        ]
        guard Set(meshObject.keys) == completedMeshKeys || Set(meshObject.keys) == completedMeshKeys.union(["mesh_sha256"]) else {
            throw invalidFixed("fixed completed mesh observable contains unknown or mixed fields")
        }
        try requireMeshObservable(
            meshObject,
            observable: observable,
            meshSHA: cell.world.meshSha256,
            includeMeshSHA: meshObject["mesh_sha256"] != nil
        )
        _ = completed // Identity was checked by the enclosing common verifier.
        _ = planAuthority // Kept in the call to make the plan/authority bind explicit.
        _ = cell
    }

    private func recomputeFixedPlanFacts(_ plan: [String: Any], cell: FightboxRouteCell) throws -> FixedPlanFacts {
        let expectedIDs = ["owner-home", "route-core", "transition", "residual"]
        let expectedSpacing: [Int64] = [4, 8, 16, 32]
        let summariesValue = plan["tier_summaries"]
        guard let summaries = summariesValue as? [Any], summaries.count == expectedIDs.count else {
            throw invalidFixed("fixed probe plan tier_summaries are missing or malformed")
        }
        var summaryFacts: [FixedTierFact] = []
        for (index, item) in summaries.enumerated() {
            let summary = try requiredObjectValue(item, label: "fixed plan tier summary")
            try requireExactKeys(summary, ["id", "ground_spacing_m", "analysis_spacing_m", "layers", "probe_count"], label: "fixed plan tier summary")
            guard summary["id"] as? String == expectedIDs[index],
                  integer(summary["ground_spacing_m"]) == expectedSpacing[index],
                  integer(summary["analysis_spacing_m"]) == expectedSpacing[index],
                  let probeCount = unsigned(summary["probe_count"]),
                  let layers = summary["layers"] as? [Any], !layers.isEmpty else {
                throw invalidFixed("fixed plan tier summary identity is invalid")
            }
            var layerFacts: [(id: String, upMm: Int64, spacingM: Int64, probeCount: UInt64)] = []
            var layerTotal: UInt64 = 0
            for layerItem in layers {
                let layer = try requiredObjectValue(layerItem, label: "fixed plan probe layer")
                try requireExactKeys(layer, ["id", "up_mm", "spacing_m", "probe_count"], label: "fixed plan probe layer")
                guard let id = layer["id"] as? String,
                      let up = integer(layer["up_mm"]),
                      let spacing = integer(layer["spacing_m"]),
                      let count = unsigned(layer["probe_count"]) else {
                    throw invalidFixed("fixed plan layer count is invalid")
                }
                let (total, totalOverflow) = layerTotal.addingReportingOverflow(count)
                guard !totalOverflow else { throw invalidFixed("fixed plan layer count overflows") }
                layerTotal = total
                layerFacts.append((id, up, spacing, count))
            }
            guard layerTotal == probeCount else {
                throw invalidFixed("fixed plan tier/layer counts disagree")
            }
            summaryFacts.append(FixedTierFact(id: expectedIDs[index], groundSpacingM: expectedSpacing[index], analysisSpacingM: expectedSpacing[index], layers: layerFacts, probeCount: probeCount))
        }
        guard let probes = plan["probes"] as? [Any],
              let requested = unsigned((plan["byte_estimate"] as? [String: Any])?["probe_count"]),
              requested == probes.count else {
            throw invalidFixed("fixed plan probe count is not exact")
        }
        var counts = Array(repeating: UInt64(0), count: expectedIDs.count)
        var ownerGround: [FixedPoint] = []
        var ownerHeight: Int64?
        for item in probes {
            let probe = try requiredObjectValue(item, label: "fixed plan probe")
            try requireExactKeys(probe, ["center_city_enu_mm", "radius_mm", "global_lattice_index", "tier_id", "layer_id"], label: "fixed plan probe")
            guard let tierID = probe["tier_id"] as? String,
                  let layerID = probe["layer_id"] as? String,
                  let tierIndex = expectedIDs.firstIndex(of: tierID),
                  let center = int64Array(probe["center_city_enu_mm"], count: 3) else {
                throw invalidFixed("fixed plan probe identity is invalid")
            }
            let (newCount, overflow) = counts[tierIndex].addingReportingOverflow(1)
            guard !overflow else { throw invalidFixed("fixed plan tier count overflows") }
            counts[tierIndex] = newCount
            if tierID == "owner-home" && layerID == "ground" {
                if let current = ownerHeight, current != center[2] {
                    throw invalidFixed("owner-home ground probes do not share one height")
                }
                ownerHeight = center[2]
                ownerGround.append(FixedPoint(east: center[0], north: center[1]))
            }
        }
        guard counts == summaryFacts.map(\.probeCount),
              summaryFacts[0].layers.count == 1,
              summaryFacts[0].layers[0].id == "ground",
              summaryFacts[0].layers[0].upMm == 1_500,
              summaryFacts[0].layers[0].probeCount == counts[0],
              (ownerHeight ?? 1_500) == 1_500,
              UInt64(ownerGround.count) == summaryFacts[0].probeCount else {
            throw invalidFixed("fixed plan probes do not recompute the declared tier summaries")
        }
        let tierTotal = try counts.reduce(UInt64(0)) { partial, value in
            let (sum, overflow) = partial.addingReportingOverflow(value)
            guard !overflow else { throw invalidFixed("fixed plan tier total overflows") }
            return sum
        }
        guard tierTotal == requested, requested == cell.cityBake.probeCount else {
            throw invalidFixed("fixed plan tier total disagrees with route probe count")
        }
        guard let policy = plan["policy"] as? [String: Any], let policyTiers = policy["tiers"] as? [Any], policyTiers.count == 4 else {
            throw invalidFixed("fixed plan policy tiers are missing")
        }
        for (index, item) in policyTiers.enumerated() {
            let tier = try requiredObjectValue(item, label: "fixed plan policy tier")
            guard tier["id"] as? String == expectedIDs[index], integer(tier["ground_spacing_m"]) == expectedSpacing[index] else {
                throw invalidFixed("fixed plan policy tier identity is invalid")
            }
        }
        return FixedPlanFacts(tiers: summaryFacts, ownerHeightMm: ownerHeight ?? 1_500, ownerGroundProbes: ownerGround)
    }

    private func parseTierFacts(_ value: Any?, probes: [Any]?, label: String) throws -> [FixedTierFact] {
        guard let summaries = value as? [Any], summaries.count == 4 else { throw invalidFixed("\(label) is malformed") }
        var facts: [FixedTierFact] = []
        for item in summaries {
            let summary = try requiredObjectValue(item, label: label)
            try requireExactKeys(summary, ["id", "ground_spacing_m", "analysis_spacing_m", "layers", "probe_count"], label: label)
            guard let id = summary["id"] as? String,
                  let ground = integer(summary["ground_spacing_m"]),
                  let analysis = integer(summary["analysis_spacing_m"]),
                  let count = unsigned(summary["probe_count"]),
                  let layers = summary["layers"] as? [Any] else { throw invalidFixed("\(label) has invalid values") }
            var layerFacts: [(id: String, upMm: Int64, spacingM: Int64, probeCount: UInt64)] = []
            for item in layers {
                let layer = try requiredObjectValue(item, label: "\(label) layer")
                try requireExactKeys(layer, ["id", "up_mm", "spacing_m", "probe_count"], label: "\(label) layer")
                guard let lid = layer["id"] as? String, let up = integer(layer["up_mm"]), let spacing = integer(layer["spacing_m"]), let pc = unsigned(layer["probe_count"]) else { throw invalidFixed("\(label) layer is invalid") }
                layerFacts.append((lid, up, spacing, pc))
            }
            facts.append(FixedTierFact(id: id, groundSpacingM: ground, analysisSpacingM: analysis, layers: layerFacts, probeCount: count))
        }
        _ = probes
        return facts
    }

    private func planLocalTranslation(_ plan: [String: Any], cell: FightboxRouteCell) throws -> [Int64] {
        guard let planCell = plan["cell"] as? [String: Any],
              let translation = int64Array(planCell["local_to_city_enu_mm"], count: 3) else {
            throw invalidFixed("fixed plan local translation is missing")
        }
        let expected: [Int64] = [
            Int64(cell.gridIndex.east) * 485_000,
            Int64(cell.gridIndex.north) * 485_000,
            0,
        ]
        guard translation == expected else { throw invalidFixed("fixed plan local translation disagrees with route cell") }
        return translation
    }

    private func parseFixedMesh(_ data: Data) throws -> QuantizedMesh {
        let magic = Array(data.prefix(8))
        guard magic == Array("FBXMESH\0".utf8) else { throw invalidFixed("mesh magic is invalid") }
        var cursor = 8
        let version = try readU32(data, &cursor)
        let vertexCount = Int(try readU32(data, &cursor))
        let triangleCount = Int(try readU32(data, &cursor))
        guard version == 1 else { throw invalidFixed("unsupported mesh format version") }
        let vertexBytes = try checkedByteCount(vertexCount, stride: 12, label: "mesh vertices")
        let triangleBytes = try checkedByteCount(triangleCount, stride: 16, label: "mesh triangles")
        guard 20 <= data.count, vertexBytes + triangleBytes == data.count - 20 else {
            throw invalidFixed("mesh payload length/counts are inconsistent")
        }
        var vertices: [(east: Int64, north: Int64, up: Int64)] = []
        vertices.reserveCapacity(vertexCount)
        for _ in 0..<vertexCount {
            let east = try quantizeMeshMm(try readF32(data, &cursor), field: "mesh east")
            let north = try quantizeMeshMm(try readF32(data, &cursor), field: "mesh north")
            let up = try quantizeMeshMm(try readF32(data, &cursor), field: "mesh up")
            vertices.append((east, north, up))
        }
        var triangles: [(UInt32, UInt32, UInt32)] = []
        triangles.reserveCapacity(triangleCount)
        for _ in 0..<triangleCount {
            let a = try readU32(data, &cursor), b = try readU32(data, &cursor), c = try readU32(data, &cursor)
            guard Int(a) < vertexCount, Int(b) < vertexCount, Int(c) < vertexCount else { throw invalidFixed("mesh triangle index is out of range") }
            triangles.append((a, b, c))
        }
        for _ in 0..<triangleCount { _ = try readU32(data, &cursor) } // material IDs are part of the frozen mesh format
        guard cursor == data.count else { throw invalidFixed("mesh has trailing bytes") }
        return QuantizedMesh(vertices: vertices, triangles: triangles)
    }

    private func recomputeMeshObservable(
        mesh: QuantizedMesh,
        facts: FixedPlanFacts,
        localToCityMm: [Int64]
    ) throws -> (ownerHeightMm: Int64, ownerCount: UInt64, wallEdges: UInt64, blockedPairs: UInt64, openPairs: UInt64) {
        var edges = Set<FixedEdge>()
        for triangle in mesh.triangles {
            let vertices = [mesh.vertices[Int(triangle.0)], mesh.vertices[Int(triangle.1)], mesh.vertices[Int(triangle.2)]]
            let zs = vertices.map(\.up)
            guard let minZ = zs.min(), let maxZ = zs.max() else { throw invalidFixed("mesh triangle has no vertices") }
            if minZ <= facts.ownerHeightMm && facts.ownerHeightMm <= maxZ {
                let xy = vertices.map { FixedPoint(east: $0.east, north: $0.north) }
                edges.insert(FixedEdge(xy[0], xy[1]))
                edges.insert(FixedEdge(xy[1], xy[2]))
                edges.insert(FixedEdge(xy[2], xy[0]))
            }
        }
        edges = Set(edges.filter { $0.left != $0.right })
        var ownerLocal: [FixedPoint] = []
        ownerLocal.reserveCapacity(facts.ownerGroundProbes.count)
        for probe in facts.ownerGroundProbes {
            let (east, eastOverflow) = probe.east.subtractingReportingOverflow(localToCityMm[0])
            let (north, northOverflow) = probe.north.subtractingReportingOverflow(localToCityMm[1])
            guard !eastOverflow, !northOverflow else { throw invalidFixed("owner probe local coordinate overflows") }
            ownerLocal.append(FixedPoint(east: east, north: north))
        }
        var blocked: UInt64 = 0
        var open: UInt64 = 0
        for (index, left) in ownerLocal.enumerated() {
            for (otherIndex, right) in ownerLocal.enumerated() where index != otherIndex {
                var isBlocked = false
                for edge in edges where try closedSegmentsIntersect(left, right, edge.left, edge.right) {
                    isBlocked = true
                    break
                }
                if isBlocked {
                    let (value, overflow) = blocked.addingReportingOverflow(1); guard !overflow else { throw invalidFixed("blocked pair count overflows") }; blocked = value
                } else {
                    let (value, overflow) = open.addingReportingOverflow(1); guard !overflow else { throw invalidFixed("open pair count overflows") }; open = value
                }
            }
        }
        let ownerCount = UInt64(ownerLocal.count)
        let (pairCount, pairOverflow) = ownerCount.multipliedReportingOverflow(by: ownerCount > 0 ? ownerCount - 1 : 0)
        guard !pairOverflow, blocked.addingReportingOverflow(open).overflow == false, blocked + open == pairCount else {
            throw invalidFixed("mesh open/blocked pair accounting is invalid")
        }
        return (facts.ownerHeightMm, ownerCount, UInt64(edges.count), blocked, open)
    }

    private func requireMeshObservable(
        _ object: [String: Any],
        observable: (ownerHeightMm: Int64, ownerCount: UInt64, wallEdges: UInt64, blockedPairs: UInt64, openPairs: UInt64),
        meshSHA: String?,
        includeMeshSHA: Bool
    ) throws {
        guard object["algorithm"] as? String == "canonical-package-mesh-owner-ground-open-pairs-v2",
              object["coordinate_encoding"] as? String == "signed_integer_millimetres",
              integer(object["owner_ground_height_mm"]) == observable.ownerHeightMm,
              unsigned(object["owner_ground_probe_count"]) == observable.ownerCount,
              unsigned(object["wall_edge_count"]) == observable.wallEdges,
              unsigned(object["blocked_owner_ground_ordered_pair_count"]) == observable.blockedPairs,
              unsigned(object["open_owner_ground_ordered_pair_count"]) == observable.openPairs else {
            throw invalidFixed("mesh observable does not match the package-derived recomputation")
        }
        if includeMeshSHA {
            guard object["mesh_sha256"] as? String == meshSHA, isSHA256(object["mesh_sha256"]) else { throw invalidFixed("estimate mesh observable is not bound to mesh SHA-256") }
        } else if object["mesh_sha256"] != nil {
            throw invalidFixed("completed mesh observable carries a mixed estimate-only mesh_sha256 field")
        }
    }

    private func canonicalFixedRequestSHA(_ request: [String: Any]) -> String? {
        guard let mesh = request["mesh_sha256"] as? String,
              let materials = request["materials_sha256"] as? String,
              let plan = request["probe_plan_sha256"] as? String?,
              let placement = request["placement_policy_sha256"] as? String?,
              let layout = request["probe_layout_sha256"] as? String?,
              let probeCount = integer(request["probe_count"]),
              let horizon = integer(request["path_horizon_m"]),
              let pathing = request["pathing"] as? [String: Any],
              let visibilityRange = f32JSON(pathing["visibility_range_m"]),
              let visibilitySamples = integer(pathing["visibility_samples"]),
              let threshold = f32JSON(pathing["visibility_threshold"]),
              let radius = f32JSON(pathing["probe_visibility_radius_m"]),
              let threads = integer(pathing["threads"]),
              let sdk = request["sdk"] as? [String: Any],
              let metadataSchema = sdk["metadata_schema"] as? String,
              let steamVersion = sdk["steam_audio_version"] as? String,
              let upstream = sdk["upstream_commit"] as? String,
              let baker = sdk["baker_revision"] as? String else { return nil }
        let optionalJSON: (String?) -> String = { value in
            guard let value else { return "null" }
            return "\"\(value)\""
        }
        let canonical = "{\"mesh_sha256\":\"\(mesh)\",\"materials_sha256\":\"\(materials)\",\"probe_plan_sha256\":\(optionalJSON(plan)),\"placement_policy_sha256\":\(optionalJSON(placement)),\"probe_layout_sha256\":\(optionalJSON(layout)),\"probe_count\":\(probeCount),\"path_horizon_m\":\(horizon),\"pathing\":{\"visibility_range_m\":\(visibilityRange),\"visibility_samples\":\(visibilitySamples),\"visibility_threshold\":\(threshold),\"probe_visibility_radius_m\":\(radius),\"threads\":\(threads)},\"sdk\":{\"metadata_schema\":\"\(metadataSchema)\",\"steam_audio_version\":\"\(steamVersion)\",\"upstream_commit\":\"\(upstream)\",\"baker_revision\":\"\(baker)\"}}"
        var bytes = Data("fightbox.probe-byte-estimate.request.v2\0".utf8)
        bytes.append(contentsOf: canonical.utf8)
        return SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
    }

    private func f32JSON(_ value: Any?) -> String? {
        guard let value = double(value), let float = Float(exactly: value), float.isFinite else { return nil }
        var text = String(format: "%.9g", Double(float))
        if !text.contains(".") && !text.contains("e") && !text.contains("E") { text += ".0" }
        if let exponent = text.firstIndex(where: { $0 == "e" || $0 == "E" }) {
            let prefix = String(text[..<exponent])
            var suffix = String(text[text.index(after: exponent)...])
            let sign = suffix.first == "+" || suffix.first == "-" ? String(suffix.removeFirst()) : ""
            suffix = suffix.drop(while: { $0 == "0" }).isEmpty ? "0" : String(suffix.drop(while: { $0 == "0" }))
            text = prefix + "e" + sign + suffix
        }
        return text
    }

    private func fixedPointBytes(owner: UInt64, routeCore: UInt64, transition: UInt64, residual: UInt64, openPairs: UInt64) throws -> UInt64 {
        var result: UInt64 = 6_000
        for (count, coefficient, label) in [(owner, UInt64(3_315), "owner-home"), (routeCore, UInt64(31), "route-core"), (transition, UInt64(54), "transition"), (residual, UInt64(30), "residual"), (openPairs, UInt64(14), "open-pairs")] {
            let (term, multiplicationOverflow) = count.multipliedReportingOverflow(by: coefficient)
            guard !multiplicationOverflow else { throw invalidFixed("\(label) coefficient arithmetic overflows") }
            let (next, additionOverflow) = result.addingReportingOverflow(term)
            guard !additionOverflow else { throw invalidFixed("fixed estimator arithmetic overflows") }
            result = next
        }
        return result
    }

    private func fixedBand(_ point: UInt64, factor: UInt64, label: String) throws -> UInt64 {
        let (product, multiplicationOverflow) = point.multipliedReportingOverflow(by: factor)
        guard !multiplicationOverflow else { throw invalidFixed("fixed \(label) band overflows") }
        let (rounded, additionOverflow) = product.addingReportingOverflow(factor == 130 ? 99 : 0)
        guard !additionOverflow else { throw invalidFixed("fixed \(label) band overflows") }
        return rounded / 100
    }

    private func closedSegmentsIntersect(_ a: FixedPoint, _ b: FixedPoint, _ c: FixedPoint, _ d: FixedPoint) throws -> Bool {
        let firstC = try orientation(a, b, c), firstD = try orientation(a, b, d)
        let secondA = try orientation(c, d, a), secondB = try orientation(c, d, b)
        if (firstC == 0 && pointOnSegment(a, b, c)) || (firstD == 0 && pointOnSegment(a, b, d)) ||
            (secondA == 0 && pointOnSegment(c, d, a)) || (secondB == 0 && pointOnSegment(c, d, b)) { return true }
        return (firstC > 0) != (firstD > 0) && (secondA > 0) != (secondB > 0)
    }

    private func orientation(_ a: FixedPoint, _ b: FixedPoint, _ c: FixedPoint) throws -> Int64 {
        let (abx, o1) = b.east.subtractingReportingOverflow(a.east), (aby, o2) = b.north.subtractingReportingOverflow(a.north)
        let (acx, o3) = c.east.subtractingReportingOverflow(a.east), (acy, o4) = c.north.subtractingReportingOverflow(a.north)
        guard !o1, !o2, !o3, !o4 else { throw invalidFixed("segment orientation coordinate subtraction overflows") }
        let (left, o5) = abx.multipliedReportingOverflow(by: acy), (right, o6) = aby.multipliedReportingOverflow(by: acx)
        let (result, o7) = left.subtractingReportingOverflow(right)
        guard !o5, !o6, !o7 else { throw invalidFixed("segment orientation arithmetic overflows") }
        return result
    }

    private func pointOnSegment(_ a: FixedPoint, _ b: FixedPoint, _ point: FixedPoint) -> Bool {
        point.east >= min(a.east, b.east) && point.east <= max(a.east, b.east) &&
            point.north >= min(a.north, b.north) && point.north <= max(a.north, b.north)
    }

    private func quantizeMeshMm(_ value: Float, field: String) throws -> Int64 {
        guard value.isFinite else { throw invalidFixed("\(field) is not finite") }
        let scaled = Double(value) * 1_000.0
        guard scaled.isFinite else { throw invalidFixed("\(field) is not finite after millimetre conversion") }
        let rounded = scaled.rounded(.toNearestOrAwayFromZero)
        // Int64.max is not exactly representable as Double.  Reject the whole
        // rounded 2^63 bucket rather than relying on a saturating conversion.
        guard rounded < 9_223_372_036_854_775_808.0 && rounded >= -9_223_372_036_854_775_808.0 else {
            throw invalidFixed("\(field) overflows signed integer millimetres")
        }
        guard let result = Int64(exactly: rounded) else { throw invalidFixed("\(field) is not an integer millimetre") }
        return result
    }

    private func readU32(_ data: Data, _ cursor: inout Int) throws -> UInt32 {
        guard cursor >= 0, cursor <= data.count - 4 else { throw invalidFixed("mesh payload is truncated") }
        let value = UInt32(data[cursor]) | (UInt32(data[cursor + 1]) << 8) | (UInt32(data[cursor + 2]) << 16) | (UInt32(data[cursor + 3]) << 24)
        cursor += 4
        return value
    }

    private func readF32(_ data: Data, _ cursor: inout Int) throws -> Float {
        try Float(bitPattern: readU32(data, &cursor))
    }

    private func checkedByteCount(_ count: Int, stride: Int, label: String) throws -> Int {
        let (result, overflow) = count.multipliedReportingOverflow(by: stride)
        guard !overflow else { throw invalidFixed("\(label) byte count overflows") }
        return result
    }

    private func requireExactKeys(_ object: [String: Any], _ expected: Set<String>, label: String) throws {
        guard Set(object.keys) == expected else { throw invalidFixed("\(label) contains unknown or mixed fields") }
    }

    private func requiredObject(_ object: [String: Any], _ key: String, label: String) throws -> [String: Any] {
        try requiredObjectValue(object[key], label: label)
    }

    private func requiredObjectValue(_ value: Any?, label: String) throws -> [String: Any] {
        guard let object = value as? [String: Any] else { throw invalidFixed("\(label) must be an object") }
        return object
    }

    private func int64Array(_ value: Any?, count: Int) -> [Int64]? {
        guard let values = value as? [Any], values.count == count else { return nil }
        let result = values.compactMap(integer)
        return result.count == count ? result : nil
    }

    private func isSHA256(_ value: Any?) -> Bool {
        guard let value = value as? String, value.count == 64 else { return false }
        return value.unicodeScalars.allSatisfy { scalar in
            (scalar.value >= 48 && scalar.value <= 57) || (scalar.value >= 97 && scalar.value <= 102)
        }
    }

    private func invalidFixed(_ message: String) -> FightboxRouteManifestError {
        .invalid("fixed estimator: \(message)")
    }

    private func requireExactRegularFileClosure(
        _ root: URL,
        expected: Set<String>,
        label: String
    ) throws {
        let keys: [URLResourceKey] = [.isRegularFileKey, .isDirectoryKey, .isSymbolicLinkKey]
        let rootValues = try root.resourceValues(forKeys: Set(keys))
        guard rootValues.isDirectory == true, rootValues.isSymbolicLink != true else {
            throw FightboxRouteManifestError.invalid("\(label) root must be a real directory")
        }
        guard let enumerator = FileManager.default.enumerator(
            at: root,
            includingPropertiesForKeys: keys,
            options: []
        ) else {
            throw FightboxRouteManifestError.invalid("\(label) cannot be enumerated")
        }
        let canonicalRoot = root.resolvingSymlinksInPath().standardizedFileURL
        let rootPrefix = canonicalRoot.path.hasSuffix("/")
            ? canonicalRoot.path : canonicalRoot.path + "/"
        var delivered = Set<String>()
        for case let url as URL in enumerator {
            let values = try url.resourceValues(forKeys: Set(keys))
            guard values.isSymbolicLink != true else {
                throw FightboxRouteManifestError.invalid("\(label) contains a symbolic link")
            }
            if values.isDirectory == true { continue }
            let canonicalURL = url.resolvingSymlinksInPath().standardizedFileURL
            guard values.isRegularFile == true, canonicalURL.path.hasPrefix(rootPrefix) else {
                throw FightboxRouteManifestError.invalid("\(label) contains a non-regular entry")
            }
            delivered.insert(String(canonicalURL.path.dropFirst(rootPrefix.count)))
        }
        guard delivered == expected else {
            throw FightboxRouteManifestError.invalid("\(label) file closure is not exact")
        }
    }

    private func loadRegularFile(_ url: URL, label: String) throws -> Data {
        let values = try url.resourceValues(forKeys: [.isRegularFileKey])
        guard values.isRegularFile == true else {
            throw FightboxRouteManifestError.invalid("\(label) is absent or not a regular file")
        }
        return try Data(contentsOf: url, options: [.mappedIfSafe])
    }

    private func requireDigest(_ data: Data, expected: String, label: String) throws {
        let delivered = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        guard delivered == expected else {
            throw FightboxRouteManifestError.invalid("\(label) SHA-256 disagrees with the route manifest")
        }
    }

    private func jsonObject(_ data: Data, label: String) throws -> [String: Any] {
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw FightboxRouteManifestError.invalid("\(label) must be a JSON object")
        }
        return object
    }

    private func packageBoundsAreWithinRoute(
        _ packageCell: [String: Any],
        cell: FightboxRouteCell
    ) -> Bool {
        guard let bounds = packageCell["bounds_local_enu_m"] as? [String: Any],
              let min = doubleArray(bounds["min_enu_m"], count: 3),
              let max = doubleArray(bounds["max_enu_m"], count: 3),
              min[0] < max[0], min[1] < max[1], min[2] < max[2] else { return false }
        let cityMinEastMm = (min[0] + cell.localToCityEnuM[0]) * 1_000.0
        let cityMinNorthMm = (min[1] + cell.localToCityEnuM[1]) * 1_000.0
        let cityMaxEastMm = (max[0] + cell.localToCityEnuM[0]) * 1_000.0
        let cityMaxNorthMm = (max[1] + cell.localToCityEnuM[1]) * 1_000.0
        let halo = cell.geometryHaloBoundsCityEnuMm
        return cityMinEastMm >= Double(halo.min[0])
            && cityMinNorthMm >= Double(halo.min[1])
            && cityMaxEastMm <= Double(halo.max[0])
            && cityMaxNorthMm <= Double(halo.max[1])
    }

    private func integer(_ value: Any?) -> Int64? {
        guard let number = value as? NSNumber,
              CFGetTypeID(number) != CFBooleanGetTypeID() else { return nil }
        let text = number.stringValue
        guard !text.contains("."), !text.contains("e"), !text.contains("E") else { return nil }
        return Int64(text)
    }

    private func unsigned(_ value: Any?) -> UInt64? {
        guard let value = integer(value), value >= 0 else { return nil }
        return UInt64(value)
    }

    private func double(_ value: Any?) -> Double? {
        guard let number = value as? NSNumber,
              CFGetTypeID(number) != CFBooleanGetTypeID(),
              number.doubleValue.isFinite else { return nil }
        return number.doubleValue
    }

    private func doubleArray(_ value: Any?, count: Int) -> [Double]? {
        guard let values = value as? [Any], values.count == count else { return nil }
        let delivered = values.compactMap(double)
        return delivered.count == count ? delivered : nil
    }
}

#if os(iOS)
public struct FightboxRouteDescriptorContext: Sendable {
    public let programs: [FightboxSpatialSourceProgram]
    public let environmentalOrder: UInt32
    public let quality: FightboxSpatialQuality
    public let defaultSourceLevelDB: Float
    public let listenerPose: FightboxPose
    /// Absolute city-ENU position for route ownership; it is never inferred
    /// from a backend-local listener pose.
    public let listenerCityPositionEnuMm: SIMD2<Int64>
    public let listenerLinearVelocityMPS: SIMD3<Float>
    public let sources: [FightboxSpatialSourceState]

    public init(
        programs: [FightboxSpatialSourceProgram],
        environmentalOrder: UInt32 = 2,
        quality: FightboxSpatialQuality,
        defaultSourceLevelDB: Float = 0,
        listenerPose: FightboxPose,
        listenerCityPositionEnuMm: SIMD2<Int64>,
        listenerLinearVelocityMPS: SIMD3<Float> = .zero,
        sources: [FightboxSpatialSourceState]
    ) {
        self.programs = programs
        self.environmentalOrder = environmentalOrder
        self.quality = quality
        self.defaultSourceLevelDB = defaultSourceLevelDB
        self.listenerPose = listenerPose
        self.listenerCityPositionEnuMm = listenerCityPositionEnuMm
        self.listenerLinearVelocityMPS = listenerLinearVelocityMPS
        self.sources = sources
    }
}

public enum FightboxRouteHostRequestResult: Sendable, Equatable {
    case noIncidentNeighbor
    case requested(cellId: String, result: FightboxCellPrepareRequestResult)
}

public enum FightboxRouteHostError: Error, Sendable, Equatable {
    case activeCityMismatch(expected: String, delivered: String)
    case listenerOutsideRoute(eastMm: Int64, northMm: Int64)
    case activeCellDoesNotOwnListener(activeCell: String, ownerCell: String)
    case routeChangedWhileVerifying(
        expected: FightboxCellIdentity,
        delivered: FightboxCellIdentity
    )
}

/// Production control-plane consumer. Selection yields one physical neighbor,
/// artifact verification runs off the actor, and only the verified descriptor
/// is fed into the coordinator that owns the one-preparation/two-world ceiling.
public actor FightboxCityRouteHost {
    private let selector: FightboxRouteNeighborSelector
    private let resolver: FightboxRouteArtifactResolver
    private let coordinator: FightboxCellStreamingCoordinator

    public init(
        selector: FightboxRouteNeighborSelector,
        resolver: FightboxRouteArtifactResolver,
        coordinator: FightboxCellStreamingCoordinator
    ) {
        self.selector = selector
        self.resolver = resolver
        self.coordinator = coordinator
    }

    public func requestDirectionalNeighbor(
        velocityEastMmPerSecond: Double,
        velocityNorthMmPerSecond: Double,
        context: FightboxRouteDescriptorContext
    ) async throws -> FightboxRouteHostRequestResult {
        let telemetry = await coordinator.telemetry()
        try requireRouteCity(telemetry.active.city)
        try requireActiveOwner(
            telemetry.active,
            cityPositionEnuMm: context.listenerCityPositionEnuMm
        )
        guard let neighbor = try selector.directionalNeighbor(
            activeCellId: telemetry.active.cell,
            velocityEastMmPerSecond: velocityEastMmPerSecond,
            velocityNorthMmPerSecond: velocityNorthMmPerSecond
        ) else { return .noIncidentNeighbor }
        return try await verifyAndRequest(
            neighbor: neighbor,
            expectedActiveIdentity: telemetry.active,
            context: context
        )
    }

    public func requestAuthoredNeighbor(
        direction: FightboxAuthoredRouteDirection,
        context: FightboxRouteDescriptorContext
    ) async throws -> FightboxRouteHostRequestResult {
        let telemetry = await coordinator.telemetry()
        try requireRouteCity(telemetry.active.city)
        try requireActiveOwner(
            telemetry.active,
            cityPositionEnuMm: context.listenerCityPositionEnuMm
        )
        guard let neighbor = try selector.authoredNeighbor(
            activeCellId: telemetry.active.cell,
            direction: direction
        ) else { return .noIncidentNeighbor }
        return try await verifyAndRequest(
            neighbor: neighbor,
            expectedActiveIdentity: telemetry.active,
            context: context
        )
    }

    private func verifyAndRequest(
        neighbor: FightboxRouteCell,
        expectedActiveIdentity: FightboxCellIdentity,
        context: FightboxRouteDescriptorContext
    ) async throws -> FightboxRouteHostRequestResult {
        let resolver = self.resolver
        let verified = try await Task.detached(priority: .utility) {
            try resolver.verify(cellId: neighbor.cellId)
        }.value
        let latestTelemetry = await coordinator.telemetry()
        let latest = latestTelemetry.active
        guard latest == expectedActiveIdentity else {
            throw FightboxRouteHostError.routeChangedWhileVerifying(
                expected: expectedActiveIdentity,
                delivered: latest
            )
        }
        let descriptor = verified.makeDescriptor(
            programs: context.programs,
            environmentalOrder: context.environmentalOrder,
            quality: context.quality,
            defaultSourceLevelDB: context.defaultSourceLevelDB,
            listenerPose: context.listenerPose,
            listenerLinearVelocityMPS: context.listenerLinearVelocityMPS,
            sources: context.sources
        )
        let result = await coordinator.requestNeighbor(
            descriptor,
            expectedActiveIdentity: expectedActiveIdentity
        )
        return .requested(cellId: verified.cellId, result: result)
    }

    private func requireActiveOwner(
        _ active: FightboxCellIdentity,
        cityPositionEnuMm: SIMD2<Int64>
    ) throws {
        let eastMm = cityPositionEnuMm.x
        let northMm = cityPositionEnuMm.y
        guard let owner = try selector.ownerCell(eastMm: eastMm, northMm: northMm) else {
            throw FightboxRouteHostError.listenerOutsideRoute(
                eastMm: eastMm,
                northMm: northMm
            )
        }
        guard owner.cellId == active.cell else {
            throw FightboxRouteHostError.activeCellDoesNotOwnListener(
                activeCell: active.cell,
                ownerCell: owner.cellId
            )
        }
    }

    private func requireRouteCity(_ city: String) throws {
        guard city == selector.manifest.cityId else {
            throw FightboxRouteHostError.activeCityMismatch(
                expected: selector.manifest.cityId,
                delivered: city
            )
        }
    }
}

public extension FightboxVerifiedRouteCellArtifacts {
    func makeDescriptor(
        programs: [FightboxSpatialSourceProgram],
        environmentalOrder: UInt32 = 2,
        quality: FightboxSpatialQuality,
        defaultSourceLevelDB: Float = 0,
        listenerPose: FightboxPose,
        listenerLinearVelocityMPS: SIMD3<Float> = .zero,
        sources: [FightboxSpatialSourceState]
    ) -> FightboxCellDescriptor {
        FightboxCellDescriptor(
            identity: FightboxCellIdentity(city: cityId, cell: cellId),
            packageURL: packageDirectory,
            bakeURL: bakeDirectory,
            programs: programs,
            environmentalOrder: environmentalOrder,
            quality: quality,
            defaultSourceLevelDB: defaultSourceLevelDB,
            listenerPose: listenerPose,
            listenerLinearVelocityMPS: listenerLinearVelocityMPS,
            sources: sources,
            estimate: FightboxCellPrepareEstimate(
                rawCellBytes: rawCellBytes,
                preparedResidentBytes: preparedResidentBytes,
                preparationScratchBytes: preparationScratchBytes
            )
        )
    }
}
#endif
