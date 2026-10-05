// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "FightboxKit",
    platforms: [
        .iOS(.v15),
        .macOS(.v10_15),
    ],
    products: [
        .library(name: "FightboxKit", targets: ["FightboxKit"]),
        .executable(
            name: "FightboxMacroLifecycleHarness",
            targets: ["FightboxMacroLifecycleHarness"]
        ),
    ],
    targets: [
        .target(
            name: "FightboxC",
            path: "Sources/FightboxC",
            publicHeadersPath: "include"
        ),
        .target(
            name: "FightboxKit",
            dependencies: ["FightboxC"],
            path: "Sources/FightboxKit",
            linkerSettings: [
                .linkedFramework("AudioToolbox", .when(platforms: [.iOS])),
                .linkedFramework("AVFoundation", .when(platforms: [.iOS])),
                .linkedFramework("UIKit", .when(platforms: [.iOS])),
                .linkedFramework("CoreMotion"),
                .linkedFramework("CoreLocation", .when(platforms: [.iOS])),
            ]
        ),
        .executableTarget(
            name: "FightboxMacroLifecycleHarness",
            dependencies: ["FightboxKit", "FightboxC", "FightboxCTestSupport"],
            path: "Sources/FightboxMacroLifecycleHarness"
        ),
        .target(
            name: "FightboxCTestSupport",
            path: "Tests/FightboxCTestSupport",
            publicHeadersPath: "include"
        ),
        .testTarget(
            name: "FightboxKitTests",
            dependencies: ["FightboxKit", "FightboxCTestSupport"],
            path: "Tests/FightboxKitTests"
        ),
    ]
)
