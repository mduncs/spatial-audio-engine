// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "FightboxKit",
    platforms: [
        .iOS(.v15),
    ],
    products: [
        .library(name: "FightboxKit", targets: ["FightboxKit"]),
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
                .linkedFramework("CoreMotion"),
                .linkedFramework("CoreLocation", .when(platforms: [.iOS])),
            ]
        ),
    ]
)
