#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
manifest=""
artifact_root=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --manifest) manifest="${2:-}"; shift 2 ;;
        --artifact-root) artifact_root="${2:-}"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
if [[ -z "$manifest" || ! -f "$manifest" || -z "$artifact_root" || ! -d "$artifact_root" ]]; then
    echo "usage: scripts/test-wave17-route-manifest.sh --manifest <city-route-manifest.json> --artifact-root <locality-root>" >&2
    exit 2
fi
work="$(mktemp -d "${TMPDIR:-/tmp}/fightbox-route-manifest.XXXXXX")"
trap 'rm -rf "$work"' EXIT
swiftc \
    "$repo_root/platforms/ios/FightboxKit/Sources/FightboxKit/FightboxCityRoute.swift" \
    "$repo_root/platforms/ios/FightboxKit/Sources/FightboxKit/FightboxRouteArtifactResolver.swift" \
    "$repo_root/scripts/wave17-verify-route-manifest.swift" \
    -o "$work/wave17-verify-route-manifest"
"$work/wave17-verify-route-manifest" "$manifest" "$artifact_root"
