import CryptoKit
import Foundation

/// Exact installed authority consumed by the Wave 17 iPhone qualification host.
/// The immutable route manifest is decoded before any world is constructed, and
/// every package/bake pair is verified by FightboxKit before it can be selected.
struct FightboxInstalledCityRoute: Sendable {
    static let requiredRouteID = "wave17-locality-production-fixed-tier-mesh-v2"
    static let requiredManifestSHA256 =
        "14ef017413b52cfbd25661b81aef9d3b8b505e850a339b3721352dea21313f2a"

    let rootURL: URL
    let manifestSHA256: String
    let selector: FightboxRouteNeighborSelector
    let resolver: FightboxRouteArtifactResolver
    let verifiedArtifacts: [FightboxVerifiedRouteCellArtifacts]
    let initialArtifacts: FightboxVerifiedRouteCellArtifacts

    static func loadQualificationRoute() throws -> FightboxInstalledCityRoute {
        let supportCandidates = FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        ).map {
            $0.appendingPathComponent("Fightbox", isDirectory: true)
                .appendingPathComponent("CityRoutes", isDirectory: true)
                .appendingPathComponent("active", isDirectory: true)
        }
        let bundleCandidates = [
            Bundle.main.url(forResource: "wave17-city-route", withExtension: nil),
        ].compactMap { $0 }
        guard let root = (supportCandidates + bundleCandidates).first(where: isDirectory) else {
            throw FightboxInstalledCityRouteError.missingRouteRoot(
                "Application Support/Fightbox/CityRoutes/active"
            )
        }
        return try load(rootURL: root)
    }

    static func load(rootURL: URL) throws -> FightboxInstalledCityRoute {
        let root = rootURL.standardizedFileURL
        let rootValues: URLResourceValues
        do {
            rootValues = try root.resourceValues(forKeys: [
                .isDirectoryKey,
                .isSymbolicLinkKey,
            ])
        } catch {
            throw FightboxInstalledCityRouteError.invalidFile(
                "route root metadata could not be read: \(error.localizedDescription)"
            )
        }
        guard rootValues.isDirectory == true, rootValues.isSymbolicLink != true else {
            throw FightboxInstalledCityRouteError.invalidFile(
                "route root must be a real non-symlink directory"
            )
        }
        let manifestURL = root
            .appendingPathComponent("route", isDirectory: true)
            .appendingPathComponent("city-route-manifest.json", isDirectory: false)
        let manifestData = try loadRegularFile(
            manifestURL,
            label: "city route manifest"
        )
        let manifestSHA256 = SHA256.hash(data: manifestData)
            .map { String(format: "%02x", $0) }
            .joined()
        guard manifestSHA256 == requiredManifestSHA256 else {
            throw FightboxInstalledCityRouteError.manifestDigestMismatch(
                expected: requiredManifestSHA256,
                delivered: manifestSHA256
            )
        }

        let manifest = try FightboxCityRouteManifest.decodeStrict(manifestData)
        guard manifest.productionEligible == true,
              manifest.routeId == requiredRouteID
        else {
            throw FightboxInstalledCityRouteError.wrongRoute(
                expected: requiredRouteID,
                delivered: manifest.routeId
            )
        }
        let selector = try FightboxRouteNeighborSelector(manifest: manifest)
        let locations = Dictionary(uniqueKeysWithValues: manifest.cells.map { cell in
            let stem = "e\(cell.gridIndex.east)-n\(cell.gridIndex.north)"
            return (
                cell.cellId,
                FightboxRouteCellArtifactLocation(
                    packageDirectory: root
                        .appendingPathComponent("packages", isDirectory: true)
                        .appendingPathComponent("\(stem).fightbox", isDirectory: true),
                    bakeDirectory: root
                        .appendingPathComponent("bakes", isDirectory: true)
                        .appendingPathComponent("\(stem).baked", isDirectory: true)
                )
            )
        })
        let resolver = try FightboxRouteArtifactResolver(
            selector: selector,
            locationsByCellId: locations
        )
        guard manifest.cells.first != nil else {
            throw FightboxInstalledCityRouteError.emptyRoute
        }
        let verified = try manifest.cells.map { cell in
            try resolver.verify(cellId: cell.cellId)
        }
        guard let initial = verified.first else {
            throw FightboxInstalledCityRouteError.emptyRoute
        }
        return FightboxInstalledCityRoute(
            rootURL: root,
            manifestSHA256: manifestSHA256,
            selector: selector,
            resolver: resolver,
            verifiedArtifacts: verified,
            initialArtifacts: initial
        )
    }

    private static func loadRegularFile(_ url: URL, label: String) throws -> Data {
        let values: URLResourceValues
        do {
            values = try url.resourceValues(forKeys: [
                .isRegularFileKey,
                .isSymbolicLinkKey,
            ])
        } catch {
            throw FightboxInstalledCityRouteError.invalidFile(
                "\(label) metadata could not be read: \(error.localizedDescription)"
            )
        }
        guard values.isRegularFile == true, values.isSymbolicLink != true else {
            throw FightboxInstalledCityRouteError.invalidFile(
                "\(label) must be a regular non-symlink file"
            )
        }
        do {
            return try Data(contentsOf: url, options: [.mappedIfSafe])
        } catch {
            throw FightboxInstalledCityRouteError.invalidFile(
                "\(label) could not be read: \(error.localizedDescription)"
            )
        }
    }

    private static func isDirectory(_ url: URL) -> Bool {
        var directory: ObjCBool = false
        return FileManager.default.fileExists(atPath: url.path, isDirectory: &directory)
            && directory.boolValue
    }
}

enum FightboxInstalledCityRouteError: Error, CustomStringConvertible {
    case missingRouteRoot(String)
    case invalidFile(String)
    case manifestDigestMismatch(expected: String, delivered: String)
    case wrongRoute(expected: String, delivered: String)
    case emptyRoute

    var description: String {
        switch self {
        case let .missingRouteRoot(path):
            return "Install the exact Wave 17 route at \(path)"
        case let .invalidFile(detail):
            return detail
        case let .manifestDigestMismatch(expected, delivered):
            return "Wave 17 route manifest SHA-256 mismatch (expected \(expected), received \(delivered))"
        case let .wrongRoute(expected, delivered):
            return "Expected production route \(expected), received \(delivered)"
        case .emptyRoute:
            return "The installed production route contains no cells"
        }
    }
}
