@testable import CityMapKit
import CoreGraphics
import Foundation
import XCTest

final class LinkCommandTests: XCTestCase {
    private func object(_ line: String) throws -> NSDictionary {
        try XCTUnwrap(JSONSerialization.jsonObject(with: Data(line.utf8)) as? NSDictionary)
    }

    /// The same lines the Workbench's `parses_every_request_the_map_sends` parses.
    func testEncodesTheLinesTheWorkbenchParses() throws {
        let cases: [(LinkCommand, String)] = [
            (.move(id: "music", eastM: 12.5, northM: -3, aboveTopM: nil, done: false),
             #"{"type":"move","id":"music","east_m":12.5,"north_m":-3,"done":false}"#),
            (.move(id: "bells", eastM: 1, northM: 2, aboveTopM: 8, done: true),
             #"{"type":"move","id":"bells","east_m":1,"north_m":2,"above_top_m":8,"done":true}"#),
            (.setOn(id: "helicopter", on: true), #"{"type":"set_on","id":"helicopter","on":true}"#),
            (.spot(id: "music", key: "sears-tower"), #"{"type":"spot","id":"music","key":"sears-tower"}"#),
            (.fire(id: "helicopter"), #"{"type":"fire","id":"helicopter"}"#),
            (.moveYou(eastM: 4, northM: -2.5), #"{"type":"move_you","east_m":4,"north_m":-2.5}"#),
            (.select(id: "bells"), #"{"type":"select","id":"bells"}"#),
            (.playAll, #"{"type":"play_all"}"#),
            (.stopAll, #"{"type":"stop_all"}"#),
            (.setVolume(db: 12), #"{"type":"set_volume","db":12}"#),
        ]
        for (command, golden) in cases {
            let line = command.line()
            XCTAssertFalse(line.contains("\n"))
            XCTAssertEqual(try object(line), try object(golden), line)
        }
    }

    func testDecodesWorkbenchMessages() async throws {
        let hello = #"""
        {"type":"hello","protocol":1,"origin":{"latitude_deg":41.9656215,"longitude_deg":-87.6729939},
         "bounds_m":[[-10,-10],[10,10]],"volume":{"min_db":-20,"max_db":40},
         "dots":{"source":"baked probes","spacing_m":4,"points":[[0,0,1],[4,0,0.4]]},
         "spots":[{"key":"overhead","label":"Overhead","detail":"80 m above you","east_m":0,"north_m":0,
                   "above_top_m":80,"up_m":80,"follows_listener":true}],
         "sources":[{"id":"music","label":"Music","color":[255,184,77],"moving":false}]}
        """#
        let state = #"""
        {"type":"state","listener":{"position_m":[1,2,1.5],"yaw_deg":90},"place":{"here":"A St","near":"B St 40 m N"},
         "facing":"E","selected":"music","volume_db":30,"editable":true,"can_play":false,
         "sources":[{"id":"music","position_m":[5,2,3],"on":true,"covered":true,"size":"Party PA","spl_db":115,
                     "width_m":4,"reach_m":471,"distance_m":4.3,"direction":"ahead","level_db":93}]}
        """#
        guard case let .hello(decoded) = try Inbound.decode(Data(hello.utf8)) else { return XCTFail("hello") }
        XCTAssertEqual(decoded.dots.points.count, 2)
        XCTAssertEqual(decoded.spots.first?.followsListener, true)
        XCTAssertEqual(decoded.frame, GeoFrame(latitude: 41.9656215, longitude: -87.6729939))
        guard case let .state(live) = try Inbound.decode(Data(state.utf8)) else { return XCTFail("state") }
        XCTAssertEqual(live.sources.first?.levelDb, 93)
        guard case let .notice(text) = try Inbound.decode(Data(#"{"type":"notice","text":"held"}"#.utf8)) else {
            return XCTFail("notice")
        }
        XCTAssertEqual(text, "held")

        let store = await MainActor.run { MapStore(hello: decoded, state: live, take: .glow) }
        let sounds = await MainActor.run { store.sounds }
        XCTAssertEqual(sounds.count, 1)
        XCTAssertEqual(sounds.first?.selected, true)
        XCTAssertEqual(sounds.first?.color, RGB(bytes: [255, 184, 77]))
    }
}

final class GeoFrameTests: XCTestCase {
    /// Matches the Workbench's `scene_geo_matches_the_city_build_projection`.
    func testRoundTripsAndMatchesTheCityBuildProjection() {
        let frame = GeoFrame(latitude: 41.9656215, longitude: -87.6729939)
        let coordinate = frame.coordinate(east: 100, north: -50)
        let radius = 6_371_008.8
        XCTAssertEqual(coordinate.latitude, 41.9656215 - 50 / radius * 180 / .pi, accuracy: 1e-12)
        XCTAssertEqual(coordinate.longitude,
                       -87.6729939 + 100 / (radius * cos(41.9656215 * .pi / 180)) * 180 / .pi, accuracy: 1e-12)
        let back = frame.enu(coordinate)
        XCTAssertEqual(back.east, 100, accuracy: 1e-6)
        XCTAssertEqual(back.north, -50, accuracy: 1e-6)
    }
}

final class PainterTests: XCTestCase {
    struct FlatProjector: MapProjector {
        var pitchDegrees: Double { 0 }
        var headingDegrees: Double { 0 }
        func point(east: Double, north: Double) -> CGPoint? { CGPoint(x: 200 + east, y: 200 - north) }
    }

    func testEveryTakeDrawsAndReturnsDragTargets() throws {
        let sound = Sound(id: "music", label: "Music", color: RGB(1, 0.7, 0.3), moving: false, east: 30, north: 10, up: 20,
                          liveEast: 30, liveNorth: 10, on: true, covered: false, size: "Party PA", widthM: 4,
                          reachM: 120, distanceM: 31, direction: "ahead", levelDb: 90, selected: true)
        for take in Take.allCases {
            let context = try XCTUnwrap(CGContext(data: nil, width: 400, height: 400, bitsPerComponent: 8, bytesPerRow: 0,
                                                  space: CGColorSpaceCreateDeviceRGB(),
                                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
            let scene = SoundScene(listener: (0, 0), yawDegrees: 45, sounds: [sound], take: take)
            let targets = SoundPainter.draw(context, scene: scene, projector: FlatProjector(),
                                            bounds: CGRect(x: 0, y: 0, width: 400, height: 400))
            let target = try XCTUnwrap(targets.first, take.title)
            XCTAssertEqual(target.id, "music")
            // Top-down: no height lift, so head minus offset lands on the ground spot.
            XCTAssertEqual(target.head.x + target.toGround.dx, 230, accuracy: 0.01)
            XCTAssertEqual(target.head.y + target.toGround.dy, 190, accuracy: 0.01)
        }
    }
}

final class ShotTests: XCTestCase {
    /// The neighborhood scene's real artillery event from the link test,
    /// trimmed to the primary route, one facade echo and the crack.
    static let shot = #"""
    {"type":"shot","elapsed_s":0,"event":{"schema_version":1,"source_id":"artillery","source_index":3,
     "event_sequence":1,"sample_rate_hz":48000,"trigger_audio_sample":0,"trigger_audio_time_s":0,
     "source_position_enu_m":[-156.11,-39.94,1.5],"listener_position_enu_m":[39.727,0.742,1.5],
     "source_emission_time_s":0.74552,"line_of_sight":false,
     "arrivals":[
      {"kind":"routed_primary","label":"impact via street (206 m)","path_enu_m":[[-156.1,-39.9,1.5],[-110.6,-35.5,1.5],
       [-78.1,-9.8,1.5],[-38.3,-8.6,1.5],[-21.6,-7.0,1.5],[23.0,-4.3,1.5],[39.7,0.7,1.5]],"length_m":205.9,
       "path_is_prediction":false,"emission_time_s":0.746,"arrival_time_s":1.346,"arrival_direction_enu":[0.96,0.28,0],
       "band_pressure_gains":null,"path_id":null,"facade_id":null},
      {"kind":"echo","label":"echo · building face","path_enu_m":[[-156.1,-39.9,1.5],[21.2,-61.1,1.5],[23.2,-66.5,1.5],
       [39.7,0.7,1.5]],"length_m":253.6,"path_is_prediction":false,"emission_time_s":0.746,"arrival_time_s":1.485,
       "arrival_direction_enu":[0.24,0.97,0],"band_pressure_gains":[0.2,0.3,0.1],"path_id":null,"facade_id":1903636671},
      {"kind":"crack","label":"crack","path_enu_m":[[-5.4,-138.8,181.8],[39.7,0.7,1.5]],"length_m":232.4,
       "path_is_prediction":false,"emission_time_s":0.25,"arrival_time_s":0.9275,"arrival_direction_enu":[0.19,0.6,-0.78],
       "band_pressure_gains":null,"path_id":null,"facade_id":null}],
     "crack":{"flight_track_enu_m":[[1617.62,-1203.5,2122.82],[-156.11,-39.94,1.5]],
              "tangent_position_enu_m":[-5.3768,-138.8198,181.7709],"emission_time_s":0.25,
              "arrival_time_s":0.92755,"mach":1.5}}}
    """#

    static let hello = #"""
    {"type":"hello","protocol":1,"origin":{"latitude_deg":41.9656215,"longitude_deg":-87.6729939},
     "bounds_m":[[-300,-300],[300,300]],"volume":{"min_db":-20,"max_db":40},
     "dots":{"source":"baked probes","spacing_m":4,"points":[]},"spots":[],"sources":[],
     "buildings":[{"ring":[[-60,10],[-40,10],[-40,30],[-60,30],[-60,10]],"height_m":12}],
     "streets":[{"kind":"road","points":[[-200,-6],[100,-6]]}]}
    """#

    func playback() throws -> ShotPlayback {
        guard case let .shot(shot) = try Inbound.decode(Data(Self.shot.utf8)),
              case let .hello(hello) = try Inbound.decode(Data(Self.hello.utf8)) else {
            throw XCTSkip("decode failed")
        }
        return ShotPlayback(shot: shot, hello: hello, received: Date())
    }

    func testDecodesAndOrdersWhatReachesYou() throws {
        let playback = try playback()
        XCTAssertEqual(playback.moments.map(\.kind), [.crack, .boom, .echo])
        XCTAssertEqual(playback.endTime, 1.485, accuracy: 1e-9)
        // "Comes from" is the negated wave travel: the boom travels east to you.
        XCTAssertLessThan(playback.moments[1].from.x, 0)
        XCTAssertTrue(playback.readout.hasPrefix("Crack at 0.93 s, before the shell lands."), playback.readout)
        XCTAssertTrue(playback.readout.contains("Boom at 1.35 s by the street route, 6 m longer"), playback.readout)
    }

    /// The ground crack field is the engine's: it reaches you when the feed says.
    func testCrackFieldAgreesWithTheEngineAtYou() throws {
        let playback = try playback()
        let shell = try XCTUnwrap(playback.shell)
        XCTAssertEqual(shell.speed, 1.5 * 343, accuracy: 25)
        let atYou = try XCTUnwrap(shell.crackTime(at: playback.listener))
        XCTAssertEqual(atYou, 0.92755, accuracy: 0.01)
        // Right at the impact the shell lands before any cone: no crack there.
        XCTAssertNil(shell.crackTime(at: playback.impact + SIMD2(0, 0.1)))
    }

    func testEchoBloomsWhereThePathTurnsOffTheFacade() throws {
        let playback = try playback()
        let echo = try XCTUnwrap(playback.echoes.first)
        XCTAssertEqual(echo.tap.x, 23.2, accuracy: 1e-9)
        XCTAssertEqual(echo.tap.y, -66.5, accuracy: 1e-9)
        XCTAssertGreaterThan(echo.tapTime, 0.746)
        XCTAssertLessThan(echo.tapTime, 1.485)
    }

    func testBoomRunsDownTheRoutedPathOnTime() throws {
        let playback = try playback()
        let route = try XCTUnwrap(playback.routes.first)
        // A street sample on the route, 120 m along it.
        let index = try XCTUnwrap(playback.samples.indices.min { a, b in
            abs(playback.routeAt[0][a].s - 120) + playback.routeAt[0][a].lateral
                < abs(playback.routeAt[0][b].s - 120) + playback.routeAt[0][b].lateral
        })
        let s = playback.routeAt[0][index].s
        let arrives = route.emission + s / route.speed
        XCTAssertEqual(playback.strength(index, time: route.emission - 0.01, width: 2.6).boom, 0)
        XCTAssertGreaterThan(playback.strength(index, time: arrives, width: 2.6).boom, 0.5)
    }

    func testBuildingsHideTheGroundBehindThem() throws {
        let playback = try playback()
        let path = Silhouettes.path(playback.buildings, projector: PainterTests.FlatProjector(),
                                    bounds: CGRect(x: 0, y: 0, width: 400, height: 400))
        // FlatProjector: (east, north) -> (200 + east, 200 - north).
        XCTAssertTrue(path.contains(CGPoint(x: 150, y: 180), using: .winding))
        XCTAssertFalse(path.contains(CGPoint(x: 150, y: 206), using: .winding))
    }
}

final class MoveYouTests: XCTestCase {
    static let hello = #"""
    {"type":"hello","protocol":1,"origin":{"latitude_deg":41.88,"longitude_deg":-87.63},
     "bounds_m":[[-50,-50],[50,50]],"volume":{"min_db":-20,"max_db":40},
     "dots":{"source":"baked probes","spacing_m":4,"points":[[0,0,1]]},"spots":[],
     "sources":[{"id":"music","label":"Music","color":[255,184,77],"moving":false}],
     "buildings":[{"ring":[[-20,10],[20,10],[20,40],[-20,40],[-20,10]],"height_m":12}],
     "streets":[{"kind":"road","points":[[-50,0],[50,0]]}]}
    """#
    static let state = #"""
    {"type":"state","listener":{"position_m":[0,0,1.5],"yaw_deg":0},"place":{"here":"A St","near":""},
     "facing":"N","selected":"music","volume_db":0,"editable":true,"can_play":true,
     "sources":[{"id":"music","position_m":[5,2,3],"on":true,"covered":true,"size":"Party PA","spl_db":115,
                 "width_m":4,"reach_m":471,"distance_m":4.3,"direction":"ahead","level_db":93,"playhead_s":12.5}]}
    """#

    /// A tap inside a footprint answers at once and sends nothing; a tap on
    /// open ground goes to the Workbench, which snaps it onto the street.
    @MainActor
    func testATapWalksYouButNeverIntoABuilding() throws {
        guard case let .hello(hello) = try Inbound.decode(Data(Self.hello.utf8)),
              case let .state(state) = try Inbound.decode(Data(Self.state.utf8)) else { return XCTFail("decode") }
        XCTAssertNotNil(hello.building(containing: (0, 20)))
        XCTAssertNil(hello.building(containing: (0, 5)))
        let store = MapStore(hello: hello, state: state, take: .towers)
        var sent: [LinkCommand] = []
        store.outbox = { sent.append($0) }
        store.moveYou(east: 0, north: 20)
        XCTAssertEqual(sent, [])
        XCTAssertEqual(store.notice, "That's inside a building · tap a street")
        store.moveYou(east: 12.345, north: 6.789)
        XCTAssertEqual(sent, [.moveYou(eastM: 12.35, northM: 6.79)])
    }
}

final class MusicTests: XCTestCase {
    static let track = #"""
    {"type":"track","id":"music","track":{"rate_hz":10,"length_s":1,
     "bands_db":[-10,-20,-30, -10,-20,-30, -10,-20,-30, -10,-20,-30, -10,-20,-30,
                 -30,-20,-30, -30,-20,-30, -30,-20,-30, -30,-20,-30, -30,-20,-30],
     "kicks_s":[0.1,0.6]}}
    """#
    static let field = #"""
    {"type":"field","id":"music","source_m":[5,2,3],
     "dots":[[5,6,-12,-12,-12,4],[40,2,-31,-31,-32,35],[0,30,-40,-60,-90,343]]}
    """#
    static let paths = #"""
    {"type":"paths","id":"music","listener_m":[60,40,1.5],"line_of_sight":false,
     "primary":{"points":[[5,2,3],[60,2,3],[60,40,1.5]],"band_db":[-40,-48,-62],"length_m":93},
     "echoes":[{"points":[[5,2,3],[30,20,1.5],[60,40,1.5]],"band_db":[-45,-55,-70],"length_m":67,
                "wall_m":[30,20,1.5],"facade_id":7}]}
    """#

    @MainActor
    func testTheFieldBreathesWithTheSongDelayedByItsRoute() throws {
        guard case let .hello(hello) = try Inbound.decode(Data(MoveYouTests.hello.utf8)),
              case let .state(state) = try Inbound.decode(Data(MoveYouTests.state.utf8)),
              case let .track(track) = try Inbound.decode(Data(Self.track.utf8)),
              case let .field(field) = try Inbound.decode(Data(Self.field.utf8)),
              case let .paths(paths) = try Inbound.decode(Data(Self.paths.utf8)) else { return XCTFail("decode") }
        let store = MapStore(hello: hello, state: state, take: .towers)
        store.load(track: track, field: field, paths: paths)
        let music = try XCTUnwrap(store.music["music"])
        XCTAssertEqual(store.playingMusic.map(\.id), ["music"])
        XCTAssertEqual(store.scene?.fielded, ["music"])
        // The song's own bands: loud bass in the first half, quiet after.
        let levels = music.levels
        XCTAssertGreaterThan(levels.bands(at: 0.2).x, levels.bands(at: 0.8).x + 15)
        XCTAssertEqual(levels.bands(at: 0.2).y, levels.bands(at: 0.8).y, accuracy: 1e-9)
        XCTAssertEqual(levels.kicksAgo(at: 0.65, window: 0.2).map { ($0 * 100).rounded() / 100 }, [0.05])
        // A dot 343 m away by street hears the loud half a second later.
        XCTAssertGreaterThan(music.level(2, at: 1.2, meter: 0.6).x, music.level(2, at: 1.8, meter: 0.6).x + 15)
        // Along the routed path the highs fall away at the corner, the bass keeps on.
        let flow = try XCTUnwrap(music.paths.first)
        let before = flow.level(at: 50), after = flow.level(at: 70)
        XCTAssertLessThan(before.x - after.x, 6)
        XCTAssertGreaterThan(before.z - after.z, 20)
        XCTAssertEqual(flow.level(at: flow.length).x, -40, accuracy: 0.01)
        XCTAssertEqual(music.paths.filter(\.echo).first?.wall, SIMD2(30, 20))
        // The playhead runs on from the last state while the song plays.
        let now = Date()
        XCTAssertEqual(store.musicTime("music", at: now), 12.5, accuracy: 0.5)
    }

    func testFieldAndWallsDraw() throws {
        guard case let .hello(hello) = try Inbound.decode(Data(MoveYouTests.hello.utf8)),
              case let .field(field) = try Inbound.decode(Data(Self.field.utf8)) else { return XCTFail("decode") }
        let music = MusicPlayback(field: field, hello: hello, splDb: 115, track: nil)
        XCTAssertGreaterThan(music.counts.walls, 0, "the block's street face is lit")
        let projector = PainterTests.FlatProjector()
        let screen = MusicScreen(music, projector: projector, bounds: CGRect(x: 0, y: 0, width: 400, height: 400))
        let context = try XCTUnwrap(CGContext(data: nil, width: 400, height: 400, bitsPerComponent: 8, bytesPerRow: 0,
                                              space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        MusicPainter.draw(context, music, screen: screen, projector: projector, time: 0.2)
        for look in MusicTemperament.all {
            MusicPainter.draw(context, music, screen: screen, projector: projector, time: 0.2, temperament: look)
        }
        let image = try XCTUnwrap(context.makeImage())
        XCTAssertEqual(image.width, 400)
    }

    /// Music swells and settles through an envelope instead of strobing,
    /// calm slowest, party quickest, and never in impact fire.
    func testTemperamentsBreatheInsteadOfStrobing() throws {
        guard case let .hello(hello) = try Inbound.decode(Data(MoveYouTests.hello.utf8)),
              case let .track(track) = try Inbound.decode(Data(Self.track.utf8)),
              case let .field(field) = try Inbound.decode(Data(Self.field.utf8)) else { return XCTFail("decode") }
        let music = MusicPlayback(field: field, hello: hello, splDb: 115, track: track.track)
        let raw = music.levels
        let calm = music.breathing(.calm), groove = music.breathing(.groove), party = music.breathing(.party)
        // The bass drops 20 dB at 0.5 s: the envelope lets go over a few frames.
        XCTAssertGreaterThan(groove.bands(at: 0.5).x, raw.bands(at: 0.5).x + 10)
        XCTAssertGreaterThan(calm.bands(at: 0.6).x, groove.bands(at: 0.6).x)
        XCTAssertGreaterThan(groove.bands(at: 0.6).x, party.bands(at: 0.6).x)
        // And swells back in from the loop's quiet end rather than jumping.
        XCTAssertLessThan(groove.bands(at: 0).x, raw.bands(at: 0).x - 5)
        XCTAssertEqual(groove.bands(at: 0.4).x, raw.bands(at: 0.4).x, accuracy: 1.5)
        // A steady band is untouched; kicks keep their times.
        XCTAssertEqual(groove.bands(at: 0.3).y, raw.bands(at: 0.3).y, accuracy: 1e-9)
        XCTAssertEqual(groove.kicksAgo(at: 0.65, window: 0.2), raw.kicksAgo(at: 0.65, window: 0.2))
        XCTAssertEqual(MusicTemperament.named("Groove")?.name, "groove")
        XCTAssertNil(MusicTemperament.named("violent"))
        for look in MusicTemperament.all {
            XCTAssertGreaterThan(look.bass.blue, look.bass.red, "\(look.name): bass is no fire")
            XCTAssertGreaterThanOrEqual(look.release, look.attack)
        }
    }
}

final class ProjectionTests: XCTestCase {
    /// A straight-down pinhole camera 500 m up: the ground at 1 point per metre.
    struct DownProjector: MapProjector {
        var pitchDegrees: Double { 0 }
        var headingDegrees: Double { 0 }
        var eye: SIMD3<Double>? { SIMD3(0, 0, 500) }
        func point(east: Double, north: Double) -> CGPoint? { CGPoint(x: 200 + east, y: 200 - north) }
    }

    /// Tall buildings lean out from the middle of the view as MapKit draws
    /// them, so the cut-outs that hide the street behind them do too.
    func testHeightsProjectThroughTheEye() throws {
        let projector = DownProjector()
        let below = try XCTUnwrap(projector.point(east: 0, north: 0, up: 100))
        XCTAssertEqual(below.x, 200, accuracy: 1e-9)
        XCTAssertEqual(below.y, 200, accuracy: 1e-9)
        let roof = try XCTUnwrap(projector.point(east: 100, north: 50, up: 250))
        XCTAssertEqual(roof.x, 200 + 200, accuracy: 1e-9)
        XCTAssertEqual(roof.y, 200 - 100, accuracy: 1e-9)
        // Without an eye the old straight-up estimate stays.
        let flat = PainterTests.FlatProjector()
        XCTAssertEqual(flat.point(east: 10, north: 0, up: 30), flat.point(east: 10, north: 0))
    }
}
