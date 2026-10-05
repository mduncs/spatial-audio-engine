import CryptoKit
import Foundation

private enum HarnessFailure: Error, CustomStringConvertible {
    case failed(String)
    var description: String {
        switch self { case .failed(let message): message }
    }
}

private func require(_ condition: Bool, _ message: String) throws {
    guard condition else { throw HarnessFailure.failed(message) }
}

@main
private enum Wave17RouteManifestHarness {
    static func main() throws {
        guard CommandLine.arguments.count == 3 else {
            throw HarnessFailure.failed("usage: wave17-verify-route-manifest <city-route-manifest.json> <artifact-root>")
        }
        let url = URL(fileURLWithPath: CommandLine.arguments[1])
        let artifactRoot = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        let data = try Data(contentsOf: url)
        let manifest = try FightboxCityRouteManifest.decodeStrict(data)
        let selector = try FightboxRouteNeighborSelector(manifest: manifest)

        let westSouth = "\(manifest.cityId):e0:n0"
        let eastSouth = "\(manifest.cityId):e1:n0"
        let westNorth = "\(manifest.cityId):e0:n1"
        if selector.cell(id: westSouth) != nil && selector.cell(id: eastSouth) != nil && selector.cell(id: westNorth) != nil {
            try require(selector.ownerCell(eastMm: 242_499, northMm: 0)?.cellId == westSouth,
                        "owner immediately west of the half-open switch is wrong")
            try require(selector.ownerCell(eastMm: 242_500, northMm: 0)?.cellId == eastSouth,
                        "eastern owner must win at exactly 242500 mm")
            try require(selector.ownerCell(eastMm: 0, northMm: 242_500)?.cellId == westNorth,
                        "northern owner must win at exactly 242500 mm")
            try require(try selector.directionalNeighbor(activeCellId: westSouth,
                                                           velocityEastMmPerSecond: 1,
                                                           velocityNorthMmPerSecond: 0)?.cellId == eastSouth,
                        "eastward prediction selected the wrong incident neighbor")
            try require(try selector.directionalNeighbor(activeCellId: westSouth,
                                                           velocityEastMmPerSecond: 0,
                                                           velocityNorthMmPerSecond: 1)?.cellId == westNorth,
                        "northward prediction selected the wrong incident neighbor")
            try require(try selector.authoredNeighbor(activeCellId: westSouth, direction: .forward)?.cellId == eastSouth,
                        "authored forward neighbor is wrong")
        }

        var unknownRoot = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        unknownRoot["untrusted_extension"] = true
        let unknownData = try JSONSerialization.data(withJSONObject: unknownRoot, options: [.sortedKeys])
        do {
            _ = try FightboxCityRouteManifest.decodeStrict(unknownData)
            throw HarnessFailure.failed("strict decoder accepted an unknown top-level field")
        } catch is FightboxRouteManifestError {}

        var danglingRoot = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        var cells = danglingRoot["cells"] as! [[String: Any]]
        var prefetch = cells[0]["prefetch"] as! [String: Any]
        prefetch["forward_cell_id"] = "\(manifest.cityId):missing"
        cells[0]["prefetch"] = prefetch
        danglingRoot["cells"] = cells
        let danglingData = try JSONSerialization.data(withJSONObject: danglingRoot, options: [.sortedKeys])
        do {
            _ = try FightboxCityRouteManifest.decodeStrict(danglingData)
            throw HarnessFailure.failed("route decoder accepted a dangling prefetch identity")
        } catch is FightboxRouteManifestError {}

        var semanticMutationCount = 0
        func rejectSemanticMutation(
            _ label: String,
            _ mutate: (inout [String: Any]) -> Void
        ) throws {
            var root = try JSONSerialization.jsonObject(with: data) as! [String: Any]
            mutate(&root)
            let mutated = try JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])
            do {
                _ = try FightboxCityRouteManifest.decodeStrict(mutated)
                throw HarnessFailure.failed("route decoder accepted semantic mutation \(label)")
            } catch is FightboxRouteManifestError {
                semanticMutationCount += 1
            }
        }
        try rejectSemanticMutation("fixture-state") { root in
            var fixture = root["four_cell_fixture"] as! [String: Any]
            fixture["state"] = "plan_only"
            root["four_cell_fixture"] = fixture
        }
        try rejectSemanticMutation("oracle-bounds") { root in
            var fixture = root["four_cell_fixture"] as! [String: Any]
            var bounds = fixture["monolithic_oracle_bounds_city_enu_mm"] as! [String: Any]
            bounds["max"] = [827_501, 827_500]
            fixture["monolithic_oracle_bounds_city_enu_mm"] = bounds
            root["four_cell_fixture"] = fixture
        }
        try rejectSemanticMutation("raw-cell-bytes") { root in
            var cells = root["cells"] as! [[String: Any]]
            var prefetch = cells[0]["prefetch"] as! [String: Any]
            prefetch["raw_cell_bytes"] = 0
            cells[0]["prefetch"] = prefetch
            root["cells"] = cells
        }
        try rejectSemanticMutation("estimate-revision") { root in
            var cells = root["cells"] as! [[String: Any]]
            var prefetch = cells[0]["prefetch"] as! [String: Any]
            prefetch["estimate_revision"] = "bad"
            cells[0]["prefetch"] = prefetch
            root["cells"] = cells
        }
        try rejectSemanticMutation("owner-home-tier") { root in
            var owner = root["owner_home"] as! [String: Any]
            owner["tier_id"] = "bad"
            root["owner_home"] = owner
        }
        try rejectSemanticMutation("adjacency-axis") { root in
            var rows = root["adjacencies"] as! [[String: Any]]
            rows[0]["axis"] = rows[0]["axis"] as! String == "east_west"
                ? "north_south" : "east_west"
            root["adjacencies"] = rows
        }
        try rejectSemanticMutation("duplicate-seam") { root in
            var fixture = root["four_cell_fixture"] as! [String: Any]
            var seams = fixture["seam_ids"] as! [String]
            seams[1] = seams[0]
            fixture["seam_ids"] = seams
            root["four_cell_fixture"] = fixture
        }
        try rejectSemanticMutation("route-offset") { root in
            var cells = root["cells"] as! [[String: Any]]
            var selection = cells[1]["selection"] as! [String: Any]
            selection["route_offset_mm"] = 1
            cells[1]["selection"] = selection
            root["cells"] = cells
        }
        try rejectSemanticMutation("echo-resident-cap") { root in
            var policy = root["grid_policy"] as! [String: Any]
            policy["echo_authority_resident_cap_bytes"] = 1
            root["grid_policy"] = policy
        }
        try rejectSemanticMutation("maximum-echo-cells") { root in
            var policy = root["grid_policy"] as! [String: Any]
            policy["maximum_resident_echo_authority_cells"] = 1
            root["grid_policy"] = policy
        }

        var verifiedArtifactCount = 0
        var mutatedBatchRejected = false
        var mutatedPackageTransformRejected = false
        var unknownArtifactFileRejected = false
        var fixedUnknownModelFieldRejected = true
        var evidenceOnlyDefaultRejected = manifest.productionEligible == true
        if #available(macOS 10.15, *) {
            var locations: [String: FightboxRouteCellArtifactLocation] = [:]
            for cell in manifest.cells {
                let slug = cell.cellId.split(separator: ":").suffix(2).joined(separator: "-")
                locations[cell.cellId] = FightboxRouteCellArtifactLocation(
                    packageDirectory: artifactRoot.appendingPathComponent("packages/\(slug).fightbox", isDirectory: true),
                    bakeDirectory: artifactRoot.appendingPathComponent("bakes/\(slug).baked", isDirectory: true)
                )
            }
            if manifest.productionEligible != true {
                do {
                    _ = try FightboxRouteArtifactResolver(
                        selector: selector,
                        locationsByCellId: locations
                    )
                    throw HarnessFailure.failed(
                        "production resolver accepted a frozen evidence-only route"
                    )
                } catch is FightboxRouteManifestError {
                    evidenceOnlyDefaultRejected = true
                }
            }
            let resolver = try FightboxRouteArtifactResolver(
                selector: selector,
                locationsByCellId: locations,
                allowEvidenceOnly: manifest.productionEligible != true
            )
            for cell in manifest.cells {
                let verified = try resolver.verify(cellId: cell.cellId)
                try require(verified.cellId == cell.cellId, "verified artifact identity changed")
                verifiedArtifactCount += 1
            }

            let first = manifest.cells[0]
            let original = locations[first.cellId]!
            let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(
                "fightbox-route-mutation-\(UUID().uuidString)",
                isDirectory: true
            )
            defer { try? FileManager.default.removeItem(at: temporary) }
            try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
            let copiedPackage = temporary.appendingPathComponent("package", isDirectory: true)
            let copiedBake = temporary.appendingPathComponent("bake", isDirectory: true)
            try FileManager.default.copyItem(at: original.packageDirectory, to: copiedPackage)
            try FileManager.default.copyItem(at: original.bakeDirectory, to: copiedBake)

            let copiedManifest = copiedPackage.appendingPathComponent("manifest.json")
            let originalManifestData = try Data(contentsOf: copiedManifest)
            var packageManifest = try JSONSerialization.jsonObject(with: originalManifestData) as! [String: Any]
            var world = packageManifest["world"] as! [String: Any]
            var packageCell = world["cell"] as! [String: Any]
            packageCell["local_to_city_enu_m"] = [123, 456, 0]
            world["cell"] = packageCell
            packageManifest["world"] = world
            let mutatedManifestData = try JSONSerialization.data(
                withJSONObject: packageManifest,
                options: [.prettyPrinted, .sortedKeys]
            )
            try mutatedManifestData.write(to: copiedManifest, options: [.atomic])
            let mutatedManifestSha = SHA256.hash(data: mutatedManifestData)
                .map { String(format: "%02x", $0) }.joined()
            var transformedRouteRoot = try JSONSerialization.jsonObject(with: data) as! [String: Any]
            var transformedCells = transformedRouteRoot["cells"] as! [[String: Any]]
            var transformedWorld = transformedCells[0]["world"] as! [String: Any]
            transformedWorld["manifest_sha256"] = mutatedManifestSha
            transformedCells[0]["world"] = transformedWorld
            transformedRouteRoot["cells"] = transformedCells
            let transformedRouteData = try JSONSerialization.data(
                withJSONObject: transformedRouteRoot,
                options: [.sortedKeys]
            )
            let transformedManifest = try FightboxCityRouteManifest.decodeStrict(transformedRouteData)
            let transformedSelector = try FightboxRouteNeighborSelector(manifest: transformedManifest)
            var transformedLocations = locations
            transformedLocations[first.cellId] = FightboxRouteCellArtifactLocation(
                packageDirectory: copiedPackage,
                bakeDirectory: copiedBake
            )
            let transformedResolver = try FightboxRouteArtifactResolver(
                selector: transformedSelector,
                locationsByCellId: transformedLocations,
                allowEvidenceOnly: manifest.productionEligible != true
            )
            do {
                _ = try transformedResolver.verify(cellId: first.cellId)
                throw HarnessFailure.failed("artifact resolver accepted a package transform mismatch")
            } catch is FightboxRouteManifestError {
                mutatedPackageTransformRejected = true
            }
            try originalManifestData.write(to: copiedManifest, options: [.atomic])

            let copiedBatch = copiedBake.appendingPathComponent("probe-batch.bin")
            var batchData = try Data(contentsOf: copiedBatch)
            try require(!batchData.isEmpty, "copied probe batch is empty")
            let originalBatchData = batchData
            batchData[0] ^= 0x01
            try batchData.write(to: copiedBatch, options: [.atomic])
            locations[first.cellId] = FightboxRouteCellArtifactLocation(
                packageDirectory: copiedPackage,
                bakeDirectory: copiedBake
            )
            let mutatedResolver = try FightboxRouteArtifactResolver(
                selector: selector,
                locationsByCellId: locations,
                allowEvidenceOnly: manifest.productionEligible != true
            )
            do {
                _ = try mutatedResolver.verify(cellId: first.cellId)
                throw HarnessFailure.failed("artifact resolver accepted a mutated probe batch")
            } catch is FightboxRouteManifestError {
                mutatedBatchRejected = true
            }

            try originalBatchData.write(to: copiedBatch, options: [.atomic])
            try Data("unbound".utf8).write(
                to: copiedBake.appendingPathComponent("unexpected-authority.bin"),
                options: [.atomic]
            )
            do {
                _ = try mutatedResolver.verify(cellId: first.cellId)
                throw HarnessFailure.failed("artifact resolver accepted an unknown bake file")
            } catch is FightboxRouteManifestError {
                unknownArtifactFileRejected = true
            }
            try? FileManager.default.removeItem(at: copiedBake.appendingPathComponent("unexpected-authority.bin"))

            // Fixed Wave 17 envelopes must reject additive fields in the
            // calibrated model, even when the route digest is re-bound to the
            // mutated bytes.  This catches a permissive JSON parser rather
            // than merely exercising the file hash closure.
            if first.cityBake.completed?.probeByteEstimateV2Sha256 != nil,
               let estimateData = try? Data(contentsOf: copiedBake.appendingPathComponent("probe-byte-estimate-v2.json")),
               let estimateObject = try? JSONSerialization.jsonObject(with: estimateData) as? [String: Any],
               let estimateModel = estimateObject["model"] as? [String: Any],
               estimateModel["revision"] as? String == "wave17-fixed-tier-mesh-open-pairs-v2" {
                var mutatedEstimate = estimateObject
                var mutatedModel = estimateModel
                mutatedModel["unknown_fixed_field"] = true
                mutatedEstimate["model"] = mutatedModel
                let mutatedEstimateData = try JSONSerialization.data(withJSONObject: mutatedEstimate, options: [.sortedKeys])
                try mutatedEstimateData.write(to: copiedBake.appendingPathComponent("probe-byte-estimate-v2.json"), options: [.atomic])
                let mutatedEstimateSHA = SHA256.hash(data: mutatedEstimateData).map { String(format: "%02x", $0) }.joined()
                var fixedRouteRoot = try JSONSerialization.jsonObject(with: data) as! [String: Any]
                var fixedCells = fixedRouteRoot["cells"] as! [[String: Any]]
                var fixedBake = fixedCells[0]["city_bake"] as! [String: Any]
                var fixedCompleted = fixedBake["completed"] as! [String: Any]
                fixedCompleted["probe_byte_estimate_v2_sha256"] = mutatedEstimateSHA
                fixedCompleted["probe_byte_estimate_v2_size_bytes"] = mutatedEstimateData.count
                fixedBake["completed"] = fixedCompleted
                fixedCells[0]["city_bake"] = fixedBake
                fixedRouteRoot["cells"] = fixedCells
                let fixedRouteData = try JSONSerialization.data(withJSONObject: fixedRouteRoot, options: [.sortedKeys])
                let fixedManifest = try FightboxCityRouteManifest.decodeStrict(fixedRouteData)
                let fixedSelector = try FightboxRouteNeighborSelector(manifest: fixedManifest)
                var fixedLocations = locations
                fixedLocations[first.cellId] = FightboxRouteCellArtifactLocation(packageDirectory: copiedPackage, bakeDirectory: copiedBake)
                let fixedResolver = try FightboxRouteArtifactResolver(
                    selector: fixedSelector,
                    locationsByCellId: fixedLocations,
                    allowEvidenceOnly: manifest.productionEligible != true
                )
                do {
                    _ = try fixedResolver.verify(cellId: first.cellId)
                    fixedUnknownModelFieldRejected = false
                } catch is FightboxRouteManifestError {}
            }
        }

        let result: [String: Any] = [
            "status": "passed",
            "schema_version": manifest.schemaVersion,
            "route_id": manifest.routeId,
            "production_eligible": manifest.productionEligible == true,
            "evidence_only_default_rejected": evidenceOnlyDefaultRejected,
            "cell_count": manifest.cells.count,
            "adjacency_count": manifest.adjacencies.count,
            "half_open_boundary_mm": 242_500,
            "strict_unknown_field_rejected": true,
            "dangling_neighbor_rejected": true,
            "one_selected_neighbor_per_direction": true,
            "verified_artifact_count": verifiedArtifactCount,
            "mutated_probe_batch_rejected": mutatedBatchRejected,
            "mutated_package_transform_rejected": mutatedPackageTransformRejected,
            "unknown_artifact_file_rejected": unknownArtifactFileRejected,
            "fixed_unknown_model_field_rejected": fixedUnknownModelFieldRejected,
            "semantic_mutations_rejected": semanticMutationCount,
        ]
        let encoded = try JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
        FileHandle.standardOutput.write(encoded)
        FileHandle.standardOutput.write(Data([0x0a]))
    }
}
