import CoreLocation

/// The Workbench's local frame: right-handed ENU metres about the scene
/// origin, projected equirectangularly exactly like `fightbox city build`.
public struct GeoFrame: Equatable, Sendable {
    public static let earthRadius = 6_371_008.8

    public let latitude: Double
    public let longitude: Double

    public init(latitude: Double, longitude: Double) {
        self.latitude = latitude
        self.longitude = longitude
    }

    private var metresPerDegreeNorth: Double { Self.earthRadius * .pi / 180 }
    private var metresPerDegreeEast: Double { metresPerDegreeNorth * cos(latitude * .pi / 180) }

    public func coordinate(east: Double, north: Double) -> CLLocationCoordinate2D {
        CLLocationCoordinate2D(
            latitude: latitude + north / metresPerDegreeNorth,
            longitude: longitude + east / metresPerDegreeEast)
    }

    public func enu(_ coordinate: CLLocationCoordinate2D) -> (east: Double, north: Double) {
        ((coordinate.longitude - longitude) * metresPerDegreeEast,
         (coordinate.latitude - latitude) * metresPerDegreeNorth)
    }
}

/// Compass bearing in degrees (0 = north, clockwise) as a unit ENU vector.
public func bearingVector(_ degrees: Double) -> (east: Double, north: Double) {
    (sin(degrees * .pi / 180), cos(degrees * .pi / 180))
}

public func distanceText(_ metres: Double) -> String {
    if metres >= 995 { return String(format: "%.1f km", metres / 1000) }
    if metres >= 100 { return "\(Int((metres / 10).rounded()) * 10) m" }
    return "\(Int(metres.rounded())) m"
}
