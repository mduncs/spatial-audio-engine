import Foundation

#if os(iOS) && canImport(CoreLocation)
import CoreLocation

/// The fixed geodetic origin used by ``GpsLocalEnuProvider``.
@available(iOS 15.0, *)
public struct GpsLocalEnuOrigin: Sendable, Equatable {
    public let latitudeDegrees: Double
    public let longitudeDegrees: Double
    public let altitudeM: Double

    public init(latitudeDegrees: Double, longitudeDegrees: Double, altitudeM: Double) {
        self.latitudeDegrees = latitudeDegrees
        self.longitudeDegrees = longitudeDegrees
        self.altitudeM = altitudeM
    }
}

/// One accepted Core Location fix projected into Fightbox local ENU meters.
@available(iOS 15.0, *)
public struct GpsLocalEnuReading: Sendable, Equatable {
    public let positionENU: SIMD3<Float>
    public let horizontalAccuracyM: Double
    public let verticalAccuracyM: Double?
    public let timestamp: Date

    public init(
        positionENU: SIMD3<Float>,
        horizontalAccuracyM: Double,
        verticalAccuracyM: Double?,
        timestamp: Date
    ) {
        self.positionENU = positionENU
        self.horizontalAccuracyM = horizontalAccuracyM
        self.verticalAccuracyM = verticalAccuracyM
        self.timestamp = timestamp
    }
}

@available(iOS 15.0, *)
public enum GpsLocalEnuInvalidReason: Sendable, Equatable {
    case locationServicesDisabled
    case authorizationDenied
    case invalidCoordinate
    case horizontalAccuracyM(Double)
    case locationManagerError(String)
}

/// Current usability of the GPS/local-ENU stream.
@available(iOS 15.0, *)
public enum GpsLocalEnuState: Sendable, Equatable {
    case waitingForAuthorization
    case waitingForAcceptedFix
    case valid(GpsLocalEnuReading)
    case stale(GpsLocalEnuReading)
    case invalid(GpsLocalEnuInvalidReason)

    /// The most recent projected position, including a stale position.
    public var positionENU: SIMD3<Float>? {
        switch self {
        case let .valid(reading), let .stale(reading):
            reading.positionENU
        case .waitingForAuthorization, .waitingForAcceptedFix, .invalid:
            nil
        }
    }

    /// The most recent horizontal accuracy, including a stale reading.
    public var horizontalAccuracyM: Double? {
        switch self {
        case let .valid(reading), let .stale(reading):
            reading.horizontalAccuracyM
        case .waitingForAuthorization, .waitingForAcceptedFix, .invalid:
            nil
        }
    }
}

/// A foreground Core Location provider for Fightbox local ENU listener position.
///
/// The first fresh fix whose `horizontalAccuracy` is nonnegative and no greater
/// than `maximumHorizontalAccuracyM` fixes the origin for the provider's
/// lifetime (or until ``resetOrigin()``). Subsequent accepted fixes use a
/// small-area equirectangular projection with WGS-84 equatorial radius
/// 6,378,137 m: x is east, y north, and z is altitude relative to the origin.
/// This projection is intended for a walk-sized local scene, not city-to-city
/// geodesy.
///
/// The default gate is 20 m and the default stale interval is 5 s. The provider
/// requests when-in-use authorization only, enables no background delivery,
/// and assumes no background-mode entitlement. Construct and start it from a
/// thread with a running event loop (normally the main thread).
///
/// Feed valid positions into the existing head tracker on the host's serialized
/// control path:
///
/// ```swift
/// gps.onStateChange = { state in
///     guard case let .valid(reading) = state else { return }
///     headTracker.listenerPositionENU = reading.positionENU
/// }
/// ```
@available(iOS 15.0, *)
public final class GpsLocalEnuProvider: NSObject, CLLocationManagerDelegate, @unchecked Sendable {
    public let maximumHorizontalAccuracyM: Double
    public let staleAfterSeconds: TimeInterval

    public var onStateChange: (@Sendable (GpsLocalEnuState) -> Void)?

    public var origin: GpsLocalEnuOrigin? {
        lock.withLock { storedOrigin }
    }

    /// The current state. A previously valid reading becomes `.stale` when read
    /// after `staleAfterSeconds`, even if Core Location has sent no new callback.
    public var state: GpsLocalEnuState {
        lock.withLock {
            switch storedState {
            case let .valid(reading)
                where Date().timeIntervalSince(reading.timestamp) > staleAfterSeconds:
                return .stale(reading)
            default:
                return storedState
            }
        }
    }

    private static let earthRadiusM = 6_378_137.0

    private let manager: CLLocationManager
    private let lock = NSLock()
    private var storedOrigin: GpsLocalEnuOrigin?
    private var storedState: GpsLocalEnuState = .waitingForAuthorization

    public init(
        maximumHorizontalAccuracyM: Double = 20,
        staleAfterSeconds: TimeInterval = 5
    ) {
        precondition(
            maximumHorizontalAccuracyM.isFinite && maximumHorizontalAccuracyM > 0,
            "maximumHorizontalAccuracyM must be finite and positive"
        )
        precondition(
            staleAfterSeconds.isFinite && staleAfterSeconds > 0,
            "staleAfterSeconds must be finite and positive"
        )
        self.maximumHorizontalAccuracyM = maximumHorizontalAccuracyM
        self.staleAfterSeconds = staleAfterSeconds
        manager = CLLocationManager()
        super.init()
        manager.delegate = self
        manager.desiredAccuracy = kCLLocationAccuracyBest
        manager.distanceFilter = kCLDistanceFilterNone
        manager.pausesLocationUpdatesAutomatically = true
        manager.allowsBackgroundLocationUpdates = false
    }

    public func start() {
        guard CLLocationManager.locationServicesEnabled() else {
            publish(.invalid(.locationServicesDisabled))
            return
        }

        switch manager.authorizationStatus {
        case .notDetermined:
            publish(.waitingForAuthorization)
            manager.requestWhenInUseAuthorization()
        case .restricted, .denied:
            publish(.invalid(.authorizationDenied))
        case .authorizedAlways, .authorizedWhenInUse:
            publish(.waitingForAcceptedFix)
            manager.startUpdatingLocation()
        @unknown default:
            publish(.waitingForAuthorization)
        }
    }

    public func stop() {
        manager.stopUpdatingLocation()
    }

    /// Discard the fixed origin. The next accepted fix becomes a new ENU origin.
    public func resetOrigin() {
        lock.withLock {
            storedOrigin = nil
            storedState = .waitingForAcceptedFix
        }
        onStateChange?(.waitingForAcceptedFix)
    }

    public func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        switch manager.authorizationStatus {
        case .authorizedAlways, .authorizedWhenInUse:
            publish(.waitingForAcceptedFix)
            manager.startUpdatingLocation()
        case .restricted, .denied:
            manager.stopUpdatingLocation()
            publish(.invalid(.authorizationDenied))
        case .notDetermined:
            publish(.waitingForAuthorization)
        @unknown default:
            publish(.waitingForAuthorization)
        }
    }

    public func locationManager(
        _ manager: CLLocationManager,
        didUpdateLocations locations: [CLLocation]
    ) {
        guard let location = locations.last else { return }
        consume(location)
    }

    public func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {
        publish(.invalid(.locationManagerError(String(describing: error))))
    }

    private func consume(_ location: CLLocation) {
        guard CLLocationCoordinate2DIsValid(location.coordinate) else {
            publish(.invalid(.invalidCoordinate))
            return
        }
        guard location.horizontalAccuracy >= 0,
              location.horizontalAccuracy <= maximumHorizontalAccuracyM
        else {
            publish(.invalid(.horizontalAccuracyM(location.horizontalAccuracy)))
            return
        }
        guard Date().timeIntervalSince(location.timestamp) <= staleAfterSeconds else {
            if origin != nil {
                publish(.stale(projectedReading(for: location)))
            } else {
                publish(.waitingForAcceptedFix)
            }
            return
        }

        publish(.valid(projectedReading(for: location)))
    }

    private func projectedReading(for location: CLLocation) -> GpsLocalEnuReading {
        let origin = lock.withLock { () -> GpsLocalEnuOrigin in
            if let storedOrigin {
                return storedOrigin
            }
            let fixed = GpsLocalEnuOrigin(
                latitudeDegrees: location.coordinate.latitude,
                longitudeDegrees: location.coordinate.longitude,
                altitudeM: location.altitude
            )
            storedOrigin = fixed
            return fixed
        }

        let latitudeRadians = location.coordinate.latitude * .pi / 180
        let originLatitudeRadians = origin.latitudeDegrees * .pi / 180
        var longitudeDeltaRadians =
            (location.coordinate.longitude - origin.longitudeDegrees) * .pi / 180
        if longitudeDeltaRadians > .pi {
            longitudeDeltaRadians -= 2 * .pi
        } else if longitudeDeltaRadians < -.pi {
            longitudeDeltaRadians += 2 * .pi
        }
        let latitudeDeltaRadians = latitudeRadians - originLatitudeRadians
        let meanLatitudeRadians = (latitudeRadians + originLatitudeRadians) / 2
        let eastM =
            Self.earthRadiusM * longitudeDeltaRadians * cos(meanLatitudeRadians)
        let northM = Self.earthRadiusM * latitudeDeltaRadians
        let upM = location.altitude - origin.altitudeM
        let verticalAccuracy =
            location.verticalAccuracy >= 0 ? location.verticalAccuracy : nil

        return GpsLocalEnuReading(
            positionENU: SIMD3(Float(eastM), Float(northM), Float(upM)),
            horizontalAccuracyM: location.horizontalAccuracy,
            verticalAccuracyM: verticalAccuracy,
            timestamp: location.timestamp
        )
    }

    private func publish(_ newState: GpsLocalEnuState) {
        lock.withLock {
            storedState = newState
        }
        onStateChange?(newState)
    }
}

private extension NSLock {
    func withLock<T>(_ body: () throws -> T) rethrows -> T {
        lock()
        defer { unlock() }
        return try body()
    }
}
#endif
