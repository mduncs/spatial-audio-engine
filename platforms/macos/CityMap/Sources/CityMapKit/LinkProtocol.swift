import Foundation

// The Workbench City Map link: JSON lines over loopback TCP. The Rust side
// is `tools/fightbox-workbench/src/map_link.rs`; field names match it.

/// Sent once per connection: the scene's geo frame, quality dots,
/// suggested spots and the sounds that exist.
public struct Hello: Decodable, Sendable {
    public struct Origin: Decodable, Sendable {
        public let latitudeDeg: Double
        public let longitudeDeg: Double
    }

    public struct Volume: Decodable, Sendable {
        public let minDb: Double
        public let maxDb: Double
    }

    public struct Dots: Decodable, Sendable {
        /// "baked probes", or "street grid (no probe data)".
        public let source: String
        public let spacingM: Double
        /// `[east_m, north_m, quality 0...1]`; 1 is on the baked path.
        public let points: [[Double]]
    }

    public struct Spot: Decodable, Identifiable, Sendable {
        public let key: String
        public let label: String
        public let detail: String
        public let eastM: Double
        public let northM: Double
        public let aboveTopM: Double
        public let upM: Double
        /// Placed relative to you at pick time (helicopter height).
        public let followsListener: Bool
        /// Set on famous tower tops: picking the spot raises the sound's level
        /// trim toward this SPL at 1 m until it moves again.
        public let loudSplDb: Double?
        public var id: String { key }
        public var loud: Bool { loudSplDb != nil }
    }

    public struct Source: Decodable, Sendable {
        public let id: String
        public let label: String
        public let color: [Int]
        /// Follows its own flight path; the map cannot move it.
        public let moving: Bool
    }

    /// A footprint (outer ring, ENU metres) and its roof height.
    public struct Building: Decodable, Sendable {
        public let ring: [[Double]]
        public let heightM: Double
    }

    /// A street, path or alley centreline the scene was built from.
    public struct Street: Decodable, Sendable {
        /// "major", "road", "alley" or "path".
        public let kind: String
        public let points: [[Double]]

        /// A drawn width for the kind, in metres.
        public var widthM: Double {
            switch kind {
            case "major": return 16
            case "road": return 12
            case "alley": return 6
            default: return 4.5
            }
        }
    }

    public let `protocol`: Int
    public let origin: Origin?
    public let boundsM: [[Double]]
    public let volume: Volume
    public let dots: Dots
    public let spots: [Spot]
    public let sources: [Source]
    /// Absent from older Workbenches.
    public let buildings: [Building]?
    public let streets: [Street]?

    public var frame: GeoFrame? {
        origin.map { GeoFrame(latitude: $0.latitudeDeg, longitude: $0.longitudeDeg) }
    }

    /// The footprint a ground spot falls inside, if any.
    public func building(containing point: (east: Double, north: Double)) -> Building? {
        buildings?.first { building in
            var inside = false
            let ring = building.ring
            for index in ring.indices where ring[index].count >= 2 {
                let a = ring[index], b = ring[(index + 1) % ring.count]
                guard b.count >= 2 else { continue }
                if (a[1] > point.north) != (b[1] > point.north),
                   point.east < (b[0] - a[0]) * (point.north - a[1]) / (b[1] - a[1]) + a[0] {
                    inside.toggle()
                }
            }
            return inside
        }
    }
}

/// Live listener, transport and sounds, at most ten times a second.
public struct LinkState: Decodable, Equatable, Sendable {
    public struct Listener: Decodable, Equatable, Sendable {
        public let positionM: [Double]
        public let yawDeg: Double
    }

    public struct Place: Decodable, Equatable, Sendable {
        public let here: String
        public let near: String
    }

    public struct Sound: Decodable, Equatable, Sendable {
        public let id: String
        public let positionM: [Double]
        public let on: Bool
        /// Whether the ground below is on the baked path.
        public let covered: Bool
        public let size: String
        public let splDb: Double
        public let widthM: Double
        public let reachM: Double
        public let distanceM: Double
        public let direction: String
        /// Open-air estimate at your ears, not a meter.
        public let levelDb: Double
        /// Level trim a loud spot added; 0 or absent otherwise.
        public let boostDb: Double?
        /// A song's playhead in seconds (music only).
        public let playheadS: Double?
    }

    /// The Workbench's output meter, after the limiter.
    public struct Meter: Decodable, Equatable, Sendable {
        public let rmsDbfs: Double
    }

    public let listener: Listener
    public let place: Place
    public let facing: String
    public let selected: String?
    public let volumeDb: Double
    public let meter: Meter?
    public let editable: Bool
    public let canPlay: Bool
    public let sources: [Sound]
}

/// One acoustic-feed event: what the engine planned for one trigger of a
/// sound (`tools/fightbox-workbench/src/acoustic_feed.rs`). Times are seconds
/// from the trigger; positions are ENU metres.
public struct Shot: Decodable, Sendable {
    public struct Arrival: Decodable, Sendable {
        /// "crack", "direct", "routed_primary" or "echo".
        public let kind: String
        public let label: String
        public let pathEnuM: [[Double]]
        public let lengthM: Double
        /// A reconstructed street route rather than baked geometry.
        public let pathIsPrediction: Bool
        public let emissionTimeS: Double
        public let arrivalTimeS: Double
        /// Wave travel direction at you; it comes from the opposite way.
        public let arrivalDirectionEnu: [Double]
        public let bandPressureGains: [Double]?
        public let facadeId: UInt32?
    }

    public struct Crack: Decodable, Sendable {
        /// Muzzle, then impact.
        public let flightTrackEnuM: [[Double]]
        public let tangentPositionEnuM: [Double]
        public let emissionTimeS: Double
        public let arrivalTimeS: Double
        public let mach: Double
    }

    public struct Event: Decodable, Sendable {
        public let sourceId: String
        public let sourceIndex: Int
        public let eventSequence: UInt64
        public let sourcePositionEnuM: [Double]
        public let listenerPositionEnuM: [Double]
        public let sourceEmissionTimeS: Double
        public let lineOfSight: Bool
        public let arrivals: [Arrival]
        public let crack: Crack?
    }

    /// How far past the trigger the audio already was when this was sent.
    public let elapsedS: Double
    public let event: Event
}

/// A song's own three-band level over time and its kicks, sent once.
public struct SongTrack: Decodable, Sendable {
    public struct Body: Decodable, Sendable {
        public let rateHz: Double
        public let lengthS: Double
        /// Interleaved `[low, mid, high]` dBFS, one triple per frame.
        public let bandsDb: [Double]
        public let kicksS: [Double]
    }

    public let id: String
    public let track: Body
}

/// A music source's routed level at every walkable dot near it.
public struct Field: Decodable, Sendable {
    public let id: String
    public let sourceM: [Double]
    /// `[east, north, low, mid, high, route_m]`, band levels in dB re 1 m.
    public let dots: [[Double]]
}

/// The music's paths to You: routed (or direct) primary and echoes.
public struct MusicPaths: Decodable, Sendable {
    public struct Path: Decodable, Sendable {
        public let points: [[Double]]
        /// Per band (low, mid, high) level at You, dB re 1 m from the speaker.
        public let bandDb: [Double]
        public let lengthM: Double
        /// Echoes only: where the wall returns it.
        public let wallM: [Double]?
        public let facadeId: UInt32?
    }

    public let id: String
    public let listenerM: [Double]
    public let lineOfSight: Bool
    public let primary: Path
    public let echoes: [Path]
}

public enum Inbound: Sendable {
    case hello(Hello)
    case state(LinkState)
    case shot(Shot)
    case track(SongTrack)
    case field(Field)
    case paths(MusicPaths)
    case notice(String)
    case other(String)

    private struct Kind: Decodable { let type: String }
    private struct Notice: Decodable { let text: String }

    public static func decode(_ line: Data) throws -> Inbound {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let kind = try decoder.decode(Kind.self, from: line).type
        switch kind {
        case "hello": return .hello(try decoder.decode(Hello.self, from: line))
        case "state": return .state(try decoder.decode(LinkState.self, from: line))
        case "shot": return .shot(try decoder.decode(Shot.self, from: line))
        case "track": return .track(try decoder.decode(SongTrack.self, from: line))
        case "field": return .field(try decoder.decode(Field.self, from: line))
        case "paths": return .paths(try decoder.decode(MusicPaths.self, from: line))
        case "notice": return .notice(try decoder.decode(Notice.self, from: line).text)
        default: return .other(kind)
        }
    }
}

/// Requests the map sends. The Rust test `parses_every_request_the_map_sends`
/// parses the same lines `LinkCommandTests` checks this encoder against.
public enum LinkCommand: Encodable, Equatable, Sendable {
    /// `aboveTopM` nil keeps the sound's current height above the surface.
    case move(id: String, eastM: Double, northM: Double, aboveTopM: Double?, done: Bool)
    /// One of the hello's suggested spots, by key; the Workbench places it.
    case spot(id: String, key: String)
    case setOn(id: String, on: Bool)
    /// Plays the sound again from its start: one more shot.
    case fire(id: String)
    /// Walks You to a tapped spot; the Workbench snaps it onto a street.
    case moveYou(eastM: Double, northM: Double)
    case select(id: String)
    case playAll
    case stopAll
    case setVolume(db: Double)

    private enum Key: String, CodingKey {
        case type, id, on, db, done, key
        case eastM = "east_m", northM = "north_m", aboveTopM = "above_top_m"
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: Key.self)
        switch self {
        case let .move(id, east, north, above, done):
            try container.encode("move", forKey: .type)
            try container.encode(id, forKey: .id)
            try container.encode(east, forKey: .eastM)
            try container.encode(north, forKey: .northM)
            try container.encodeIfPresent(above, forKey: .aboveTopM)
            try container.encode(done, forKey: .done)
        case let .spot(id, key):
            try container.encode("spot", forKey: .type)
            try container.encode(id, forKey: .id)
            try container.encode(key, forKey: .key)
        case let .setOn(id, on):
            try container.encode("set_on", forKey: .type)
            try container.encode(id, forKey: .id)
            try container.encode(on, forKey: .on)
        case let .fire(id):
            try container.encode("fire", forKey: .type)
            try container.encode(id, forKey: .id)
        case let .moveYou(east, north):
            try container.encode("move_you", forKey: .type)
            try container.encode(east, forKey: .eastM)
            try container.encode(north, forKey: .northM)
        case let .select(id):
            try container.encode("select", forKey: .type)
            try container.encode(id, forKey: .id)
        case .playAll:
            try container.encode("play_all", forKey: .type)
        case .stopAll:
            try container.encode("stop_all", forKey: .type)
        case let .setVolume(db):
            try container.encode("set_volume", forKey: .type)
            try container.encode(db, forKey: .db)
        }
    }

    /// One JSON line, without the trailing newline.
    public func line() -> String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        let data = (try? encoder.encode(self)) ?? Data()
        return String(decoding: data, as: UTF8.self)
    }
}
