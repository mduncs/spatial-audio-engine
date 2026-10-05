#!/usr/bin/env bash
# Compile and run the app's exact installed-route loader against a retained
# Wave 17 artifact root. This is host file-verification evidence only.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
artifact_root="${1:-}"
if [[ -z "$artifact_root" || ! -d "$artifact_root" ]]; then
  echo "usage: $0 /absolute/path/to/wave17-fixed-tier-route-root" >&2
  exit 2
fi
case "$artifact_root" in
  /*) ;;
  *)
    echo "artifact root must be absolute: $artifact_root" >&2
    exit 2
    ;;
esac

scratch="$(mktemp -d "${TMPDIR:-/tmp}/fightbox-ios-route-loader.XXXXXX")"
cleanup() {
  rm -rf -- "$scratch"
}
trap cleanup EXIT

cat >"$scratch/RouteLoaderHarness.swift" <<'SWIFT'
import Darwin
import Foundation

@main
struct RouteLoaderHarness {
    static func main() {
        do {
            try run()
        } catch {
            FileHandle.standardError.write(Data("FAIL installed route loader: \(error)\n".utf8))
            exit(1)
        }
    }

    private static func run() throws {
        guard CommandLine.arguments.count == 2 else {
            throw HarnessError.invalidArgumentCount
        }
        let route = try FightboxInstalledCityRoute.load(
            rootURL: URL(
                fileURLWithPath: CommandLine.arguments[1],
                isDirectory: true
            )
        )
        guard route.manifestSHA256 == FightboxInstalledCityRoute.requiredManifestSHA256,
              route.selector.manifest.routeId == FightboxInstalledCityRoute.requiredRouteID,
              route.verifiedArtifacts.count == 4,
              route.verifiedArtifacts.map(\.cellId) ==
                route.selector.manifest.cells.map(\.cellId)
        else {
            throw HarnessError.authorityMismatch
        }
        print(
            "PASS installed route loader " +
                "route=\(route.selector.manifest.routeId) " +
                "sha=\(route.manifestSHA256) " +
                "verified=\(route.verifiedArtifacts.count) " +
                "physical_device_proof=false"
        )
    }

    private enum HarnessError: Error {
        case invalidArgumentCount
        case authorityMismatch
    }
}
SWIFT

xcrun swiftc   -warnings-as-errors   "$repo_root/platforms/ios/FightboxKit/Sources/FightboxKit/FightboxCityRoute.swift"   "$repo_root/platforms/ios/FightboxKit/Sources/FightboxKit/FightboxRouteArtifactResolver.swift"   "$repo_root/platforms/ios/FightboxApp/Sources/FightboxInstalledCityRoute.swift"   "$scratch/RouteLoaderHarness.swift"   -o "$scratch/route-loader"
"$scratch/route-loader" "$artifact_root"
