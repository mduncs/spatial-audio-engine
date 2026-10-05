// swift-tools-version: 5.9
import PackageDescription

// City Map: live Apple Maps over the Workbench's scene, public MapKit only.
let package = Package(
    name: "CityMap",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "CityMap", targets: ["CityMap"]),
        .executable(name: "CityMapSnapshot", targets: ["CityMapSnapshot"]),
    ],
    targets: [
        .target(name: "CityMapKit"),
        .executableTarget(name: "CityMap", dependencies: ["CityMapKit"]),
        .executableTarget(name: "CityMapSnapshot", dependencies: ["CityMapKit"]),
        .testTarget(name: "CityMapKitTests", dependencies: ["CityMapKit"]),
    ]
)
