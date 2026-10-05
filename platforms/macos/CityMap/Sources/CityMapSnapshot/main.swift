// Offscreen City Map renders: Apple's map from MKMapSnapshotter (no window),
// the same dots, speaker and shot painters as the live map, and the same
// SwiftUI controls through ImageRenderer. Input is a recorded hello, state
// (and shot) from the Workbench link test.
//
// Usage:
//   CityMapSnapshot <map-hello.json> <map-state.json> <output-dir>
//   CityMapSnapshot shot <map-hello.json> <map-state.json> <map-shot.json> <output-dir> [--fps N] [--slow X]
//   (shot mode writes frames/frame-NNNN.png and named stills)
//   CityMapSnapshot music <map-hello.json> <map-state-music.json> <track> <field> <paths> <output-dir>
//                   [--fps N] [--seconds S] [--start S]
//                   [--look heat|corridors|ear|rings|columns|night] [--temper calm|groove|party]
//   (music mode: the song's real bands from its track, the busiest stretch
//   of kicks unless --start is given; --look draws one variation, --temper
//   how calm it is, groove by default as on the live map)
//   CITYMAP_DEBUG_SILHOUETTES=1 tints the building cut-outs red.

import AppKit
import CityMapKit
@preconcurrency import MapKit
import SwiftUI
import simd

var arguments = Array(CommandLine.arguments.dropFirst())
let shotMode = arguments.first == "shot"
if shotMode { arguments.removeFirst() }
let pulseMode = arguments.first == "pulse"
if pulseMode { arguments.removeFirst() }
let musicMode = arguments.first == "music"
if musicMode { arguments.removeFirst() }
func option(_ name: String, _ fallback: Double) -> Double {
    guard let index = arguments.firstIndex(of: name), arguments.indices.contains(index + 1),
          let value = Double(arguments[index + 1]) else { return fallback }
    arguments.removeSubrange(index...(index + 1))
    return value
}
let fps = option("--fps", 30)
let slow = option("--slow", 3)
let seconds = option("--seconds", 18)
let startOption = option("--start", -1)
func flag(_ name: String) -> String? {
    guard let index = arguments.firstIndex(of: name), arguments.indices.contains(index + 1) else { return nil }
    defer { arguments.removeSubrange(index...(index + 1)) }
    return arguments[index + 1]
}
/// Music variations: which layers, and whether the map goes to night.
let variation = flag("--look") ?? "standard"
let musicLayers: MusicLayers = switch variation {
case "heat": [.field, .highs, .bloom]
case "corridors": [.flow, .bloom]
case "ear": [.field, .flow, .bloom, .earCone]
case "rings": [.kicks, .bloom]
case "columns": [.columns, .bloom]
default: .standard
}
let night = variation == "night"
let temperName = flag("--temper") ?? "groove"
guard let temperament = MusicTemperament.named(temperName) else {
    FileHandle.standardError.write(Data("unknown --temper \(temperName); use calm, groove or party\n".utf8))
    exit(2)
}
guard arguments.count == (shotMode ? 4 : musicMode ? 6 : 3) else {
    FileHandle.standardError.write(Data("""
    usage: CityMapSnapshot <map-hello.json> <map-state.json> <output-dir>
           CityMapSnapshot shot <map-hello.json> <map-state.json> <map-shot.json> <output-dir> [--fps N] [--slow X]

    """.utf8))
    exit(2)
}

func load(_ path: String) throws -> Inbound {
    try Inbound.decode(Data(contentsOf: URL(fileURLWithPath: path)))
}

/// The recorded state, or for a shot the same state with only the shot's
/// sound playing (nothing else lights the streets).
func loadState(_ path: String, playing: String?) throws -> Inbound {
    guard let playing else { return try load(path) }
    var object = try JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: path))) as? [String: Any] ?? [:]
    object["selected"] = playing
    object["sources"] = (object["sources"] as? [[String: Any]] ?? []).map { source in
        var source = source
        source["on"] = (source["id"] as? String) == playing
        return source
    }
    return try Inbound.decode(JSONSerialization.data(withJSONObject: object))
}

let shotSource: String? = shotMode ? {
    guard case let .shot(shot)? = try? load(arguments[2]) else { return nil }
    return shot.event.sourceId
}() : musicMode ? {
    guard case let .field(field)? = try? load(arguments[3]) else { return nil }
    return field.id
}() : nil
guard case let .hello(hello)? = try? load(arguments[0]), let frame = hello.frame,
      case let .state(state)? = try? loadState(arguments[1], playing: shotSource)
else {
    FileHandle.standardError.write(Data("could not read the hello (with a geo origin) and state\n".utf8))
    exit(1)
}
var recordedShot: Shot?
if shotMode {
    guard case let .shot(shot)? = try? load(arguments[2]) else {
        FileHandle.standardError.write(Data("could not read the shot\n".utf8))
        exit(1)
    }
    recordedShot = shot
}
let output = URL(fileURLWithPath: arguments.last!)
try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)

let canvas = CGSize(width: 1440, height: 900)
let mapSize = CGSize(width: canvas.width - 324, height: canvas.height)

struct View3D {
    let name: String
    let distance: Double
    let pitch: Double
    let heading: Double
}

struct SnapshotProjector: MapProjector {
    let snapshot: MKMapSnapshotter.Snapshot
    let frame: GeoFrame
    let camera: View3D
    let center: (east: Double, north: Double)
    let flip: Bool
    let height: CGFloat

    var pitchDegrees: Double { camera.pitch }
    var headingDegrees: Double { camera.heading }
    var eye: SIMD3<Double>? {
        let forward = bearingVector(camera.heading)
        let back = camera.distance * sin(camera.pitch * .pi / 180)
        return SIMD3(center.east - forward.east * back, center.north - forward.north * back,
                     camera.distance * cos(camera.pitch * .pi / 180))
    }

    func point(east: Double, north: Double) -> CGPoint? {
        // Pitched snapshots wrap points behind the eye back into the frame.
        let forward = bearingVector(camera.heading)
        let back = camera.distance * sin(camera.pitch * .pi / 180)
        let along = (east - center.east) * forward.east + (north - center.north) * forward.north
        guard along > -back + 20 else { return nil }
        let raw = snapshot.point(for: frame.coordinate(east: east, north: north))
        return CGPoint(x: raw.x, y: flip ? height - raw.y : raw.y)
    }
}

@MainActor
func snapshot(_ view: View3D, center: (east: Double, north: Double), muted: Bool) throws -> MKMapSnapshotter.Snapshot {
    let options = MKMapSnapshotter.Options()
    options.size = mapSize
    options.camera = MKMapCamera(lookingAtCenter: frame.coordinate(east: center.east, north: center.north),
                                 fromDistance: view.distance, pitch: view.pitch, heading: view.heading)
    let configuration = MKStandardMapConfiguration(elevationStyle: .realistic, emphasisStyle: muted ? .muted : .default)
    configuration.pointOfInterestFilter = .excludingAll
    configuration.showsTraffic = false
    options.preferredConfiguration = configuration
    options.appearance = NSAppearance(named: .darkAqua)
    final class Box: @unchecked Sendable { var result: Result<MKMapSnapshotter.Snapshot, Error>? }
    let box = Box()
    MKMapSnapshotter(options: options).start(with: .main) { snapshot, error in
        box.result = snapshot.map { .success($0) } ?? .failure(error ?? CocoaError(.featureUnsupported))
    }
    let deadline = Date().addingTimeInterval(90)
    while box.result == nil && Date() < deadline {
        RunLoop.main.run(until: Date().addingTimeInterval(0.05))
    }
    guard let result = box.result else {
        throw CocoaError(.userCancelled, userInfo: [NSLocalizedDescriptionKey: "MapKit snapshot timed out"])
    }
    return try result.get()
}

/// One camera's snapshot with its projector and the parts that do not
/// change between frames (dots, building outlines, shot samples).
@MainActor
final class Stage {
    let shot: MKMapSnapshotter.Snapshot
    let base: CGImage
    let scale: CGFloat
    let projector: SnapshotProjector
    let bounds = CGRect(origin: .zero, size: mapSize)
    let silhouettes: CGPath
    let hello: Hello
    var shotScreen: ShotScreen?
    var musicScreen: MusicScreen?
    var musicLayers: MusicLayers = .standard
    var musicTemperament: MusicTemperament = .groove
    var night = false

    init(_ shot: MKMapSnapshotter.Snapshot, view: View3D, center: (east: Double, north: Double), hello: Hello,
         frame: GeoFrame) throws {
        self.hello = hello
        guard let base = shot.image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
            throw CocoaError(.fileReadCorruptFile)
        }
        self.shot = shot
        self.base = base
        scale = CGFloat(base.width) / mapSize.width
        // MapKit's y direction in snapshots has varied; check it against the heading.
        let ahead = bearingVector(view.heading)
        let a = shot.point(for: frame.coordinate(east: center.east, north: center.north))
        let b = shot.point(for: frame.coordinate(east: center.east + ahead.east * 30, north: center.north + ahead.north * 30))
        projector = SnapshotProjector(snapshot: shot, frame: frame, camera: view, center: center, flip: b.y > a.y,
                                      height: mapSize.height)
        silhouettes = Silhouettes.path(hello.buildings ?? [], projector: projector, bounds: CGRect(origin: .zero, size: mapSize))
    }

    func compose(store: MapStore, time: Double = 0, pulse: Double? = nil) -> CGImage? {
        guard let context = CGContext(data: nil, width: base.width, height: base.height, bitsPerComponent: 8,
                                      bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return nil }
        context.draw(base, in: CGRect(x: 0, y: 0, width: base.width, height: base.height))
        if night {
            context.setFillColor(CGColor(red: 0.01, green: 0.02, blue: 0.06, alpha: 0.62))
            context.fill(CGRect(x: 0, y: 0, width: base.width, height: base.height))
        }
        context.translateBy(x: 0, y: CGFloat(base.height))
        context.scaleBy(x: scale, y: -scale)

        // Dots, in bands of screen depth so their size follows the perspective,
        // then cut where a building stands in front of them.
        var bands: [Int: [(CGPoint, Double)]] = [:]
        var bandScale: [Int: CGFloat] = [:]
        for dot in hello.dots.points where dot.count >= 3 {
            guard let point = projector.point(east: dot[0], north: dot[1]),
                  bounds.insetBy(dx: -20, dy: -20).contains(point) else { continue }
            let band = Int(point.y / 40)
            bands[band, default: []].append((point, dot[2]))
            if bandScale[band] == nil { bandScale[band] = projector.pointsPerMetre(east: dot[0], north: dot[1]) }
        }
        // While a shot or music is up the coverage steps back so the sound reads.
        let playing = store.playingMusic
        context.saveGState()
        if store.shot != nil || !playing.isEmpty { context.setAlpha(playing.isEmpty ? 0.28 : 0.18) }
        context.beginTransparencyLayer(auxiliaryInfo: nil)
        for (band, dots) in bands {
            DotPainter.draw(context, dots: dots, spacing: CGFloat(hello.dots.spacingM) * (bandScale[band] ?? 1),
                            pixel: 1, take: store.take)
        }
        Silhouettes.cut(context, silhouettes)
        context.endTransparencyLayer()
        context.restoreGState()

        let shotTime = store.shotTime()
        if let playback = store.shot, let shotTime {
            if shotScreen == nil { shotScreen = ShotScreen(playback, projector: projector, bounds: bounds) }
            ShotPainter.drawGround(context, playback, screen: shotScreen!, time: shotTime)
        }
        for music in playing {
            if musicScreen == nil { musicScreen = MusicScreen(music, projector: projector, bounds: bounds) }
            MusicPainter.draw(context, music, screen: musicScreen!, projector: projector,
                              time: store.musicTime(music.id), meter: pulse ?? 0.6, layers: musicLayers,
                              temperament: musicTemperament)
        }
        if let scene = store.scene(time: time, pulse: pulse) {
            SoundPainter.draw(context, scene: scene, projector: projector, bounds: bounds)
        }
        if let playback = store.shot, let shotTime {
            ShotPainter.drawTop(context, playback, projector: projector, time: shotTime)
        }
        if ProcessInfo.processInfo.environment["CITYMAP_DEBUG_SILHOUETTES"] != nil {
            context.addPath(silhouettes)
            context.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 0.3))
            context.fillPath(using: .winding)
        }
        let credit = NSAttributedString(string: "Apple Maps · MapKit snapshot · © OpenStreetMap contributors", attributes: [
            .font: NSFont.systemFont(ofSize: 9.5), .foregroundColor: NSColor.white.withAlphaComponent(0.55),
        ])
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: true)
        credit.draw(at: CGPoint(x: bounds.maxX - credit.size().width - 10, y: 8))
        NSGraphicsContext.restoreGraphicsState()
        return context.makeImage()
    }
}

@MainActor
func write(store: MapStore, map: CGImage, to url: URL, scale: CGFloat) throws {
    // Drain each frame's renderer image, bitmap and PNG data here; a frame
    // loop never returns to a run loop, so they would otherwise pile up.
    try autoreleasepool {
        let mapScale = CGFloat(map.width) / mapSize.width
        let screen = CityMapScreen(store: store, scrolls: false) {
            Image(decorative: map, scale: mapScale).resizable().frame(width: mapSize.width, height: mapSize.height)
        }
        .frame(width: canvas.width, height: canvas.height)
        let renderer = ImageRenderer(content: screen)
        renderer.scale = scale
        guard let image = renderer.cgImage else { throw CocoaError(.fileWriteUnknown) }
        let rep = NSBitmapImageRep(cgImage: image)
        try rep.representation(using: .png, properties: [:])?.write(to: url)
    }
}

@MainActor
func renderViews() throws {
    let listener = (east: state.listener.positionM[0], north: state.listener.positionM[1])
    let views = [
        View3D(name: "city", distance: 900, pitch: 52, heading: 30),
        View3D(name: "close", distance: 260, pitch: 56, heading: 30),
        View3D(name: "street", distance: 150, pitch: 58, heading: 30),
    ]
    for view in views {
        // Frame the view a little ahead of you so the cone has room.
        let ahead = bearingVector(view.heading)
        let lead = view.distance * 0.08
        let center = (east: listener.east + ahead.east * lead, north: listener.north + ahead.north * lead)
        // Auto: Towers over the city, Pins up close (the live map's handoff).
        let auto: Take = view.distance > 500 ? .towers : .pins
        let takes: [(Take, String)] = view.name == "street" ? [(auto, "auto")]
            : [(auto, "auto")] + Take.allCases.map { ($0, $0.rawValue) }
        var stages: [Bool: Stage] = [:]
        for (take, name) in takes {
            let stage = try stages[take.mutedMap] ?? Stage(snapshot(view, center: center, muted: take.mutedMap),
                                                           view: view, center: center, hello: hello, frame: frame)
            stages[take.mutedMap] = stage
            let store = MapStore(hello: hello, state: state, take: take)
            guard let map = stage.compose(store: store) else { throw CocoaError(.fileWriteUnknown) }
            let url = output.appendingPathComponent("city-map-\(name)-\(view.name).png")
            try write(store: store, map: map, to: url, scale: stage.scale)
            print("wrote \(url.path)")
        }
    }
}

@MainActor
func renderShot(_ recorded: Shot) throws {
    let store = MapStore(hello: hello, state: state, take: .towers)
    store.slow = slow
    store.play(recorded)
    guard let playback = store.shot else { throw CocoaError(.fileReadCorruptFile) }
    // Look from your side toward the impact, turned a little so the impact
    // sits upper left (the mockup's framing), wide enough for every path.
    let impact = playback.impact, me = playback.listener
    let toward = atan2(impact.x - me.x, impact.y - me.y) * 180 / .pi
    var reach = simd_length(impact - me)
    for arrival in recorded.event.arrivals where arrival.kind != "crack" {
        for point in arrival.pathEnuM where point.count >= 2 {
            reach = max(reach, hypot(point[0] - (impact.x + me.x) / 2, point[1] - (impact.y + me.y) / 2) * 1.6)
        }
    }
    let heading = (toward + 30 + 360).truncatingRemainder(dividingBy: 360)
    // Canyons of towers hide their street floors at a low tilt: look down
    // more steeply where the blocks around the shot are tall.
    let middle = (impact + me) / 2
    let heights = (hello.buildings ?? []).filter { building in
        building.ring.contains { $0.count >= 2 && hypot($0[0] - middle.x, $0[1] - middle.y) < reach }
    }.map(\.heightM).sorted()
    let typical = heights.isEmpty ? 0 : heights[heights.count / 2]
    let pitch = typical > 30 ? 22.0 : typical > 18 ? 38.0 : 50.0
    let view = View3D(name: "shot", distance: min(max(440, reach * (pitch < 30 ? 1.9 : 1.6)), 1400), pitch: pitch,
                      heading: heading)
    print("shot camera: median building \(Int(typical)) m, pitch \(Int(pitch))°, \(Int(view.distance)) m out")
    // Look a little short of the middle so both ends sit above the timeline.
    let ahead = bearingVector(heading)
    let back = view.distance * (pitch < 30 ? 0.05 : 0.2)
    let center = (east: (impact.x + me.x) / 2 - ahead.east * back, north: (impact.y + me.y) / 2 - ahead.north * back)
    let stage = try Stage(snapshot(view, center: center, muted: true), view: view, center: center, hello: hello,
                          frame: frame)

    let frames = output.appendingPathComponent("frames")
    try? FileManager.default.removeItem(at: frames)
    try FileManager.default.createDirectory(at: frames, withIntermediateDirectories: true)
    let end = playback.endTime + 0.6
    let count = Int((end * slow * fps).rounded(.up)) + 1
    for index in 0..<count {
        try autoreleasepool {
            let time = Double(index) / (fps * slow)
            store.frozenShotTime = time
            guard let map = stage.compose(store: store, time: time) else { throw CocoaError(.fileWriteUnknown) }
            let url = frames.appendingPathComponent(String(format: "frame-%04d.png", index + 1))
            try write(store: store, map: map, to: url, scale: 1)
        }
    }
    print("wrote \(count) frames to \(frames.path) (\(fps) fps, slowed \(slow)x, \(String(format: "%.2f", end)) s of sound)")

    // Stills at the moments that tell the story.
    var stills: [(String, Double)] = []
    if let crack = playback.moments.first(where: { $0.kind == .crack }) {
        stills.append(("1-crack-sweeps", crack.time - 0.12))
    }
    if let boom = playback.moments.first(where: { $0.kind == .boom }) {
        stills.append(("2-boom-in-the-streets", playback.boomStart + (boom.time - playback.boomStart) * 0.55))
        stills.append(("3-boom-reaches-you", boom.time + 0.08))
    }
    if let echo = playback.moments.last(where: { $0.kind == .echo }) {
        stills.append(("4-echoes", echo.time - 0.12))
    }
    stills.append(("5-settled", end))
    for (name, time) in stills {
        store.frozenShotTime = time
        guard let map = stage.compose(store: store, time: time) else { throw CocoaError(.fileWriteUnknown) }
        let url = output.appendingPathComponent("shot-\(name).png")
        try write(store: store, map: map, to: url, scale: stage.scale)
        print("wrote \(url.path)")
    }
}

/// Music pulse: a playing sound's ground rings travel outward, their
/// strength following a 120 bpm beat (the live map follows the output meter).
@MainActor
func renderPulse() throws {
    let store = MapStore(hello: hello, state: state, take: .pins)
    guard let sound = store.sounds.first(where: \.on) else { throw CocoaError(.fileReadCorruptFile) }
    let view = View3D(name: "pulse", distance: 260, pitch: 56, heading: 30)
    let center = (east: sound.east, north: sound.north)
    let stage = try Stage(snapshot(view, center: center, muted: false), view: view, center: center, hello: hello,
                          frame: frame)
    let frames = output.appendingPathComponent("frames")
    try? FileManager.default.removeItem(at: frames)
    try FileManager.default.createDirectory(at: frames, withIntermediateDirectories: true)
    let count = Int(4 * fps)
    for index in 0..<count {
        try autoreleasepool {
            let time = Double(index) / fps
            let beat = time.truncatingRemainder(dividingBy: 0.5) / 0.5
            let pulse = 0.35 + 0.65 * exp(-beat * 5)
            guard let map = stage.compose(store: store, time: time, pulse: pulse) else { throw CocoaError(.fileWriteUnknown) }
            try write(store: store, map: map, to: frames.appendingPathComponent(String(format: "frame-%04d.png", index + 1)),
                      scale: 1)
        }
    }
    print("wrote \(count) pulse frames to \(frames.path)")
}

/// Music: the party PA's living field, streams to you, kick rings and
/// glowing walls, from the song's real band track over a real stretch.
@MainActor
func renderMusic() throws {
    guard case let .track(track)? = try? load(arguments[2]), case let .field(field)? = try? load(arguments[3]),
          case let .paths(paths)? = try? load(arguments[4]) else {
        FileHandle.standardError.write(Data("could not read the music track, field and paths\n".utf8))
        exit(1)
    }
    let store = MapStore(hello: hello, state: state, take: .towers)
    store.load(track: track, field: field, paths: paths)
    guard let music = store.music[field.id] else { throw CocoaError(.fileReadCorruptFile) }
    // The busiest stretch of kicks, unless asked.
    var start = startOption
    if start < 0 {
        var best = (0.0, -1)
        var at = 0.0
        while at + seconds < track.track.lengthS {
            let count = track.track.kicksS.filter { $0 >= at && $0 < at + seconds }.count
            if count > best.1 { best = (at, count) }
            at += 2
        }
        start = best.0
    }
    let speaker = music.source
    let me = SIMD2(paths.listenerM[0], paths.listenerM[1])
    let toward = atan2(speaker.x - me.x, speaker.y - me.y) * 180 / .pi
    let heading = (toward + 30 + 360).truncatingRemainder(dividingBy: 360)
    let middle = (speaker + me) / 2
    let reach = max(simd_length(speaker - me), 90)
    let heights = (hello.buildings ?? []).filter { building in
        building.ring.contains { $0.count >= 2 && hypot($0[0] - middle.x, $0[1] - middle.y) < 220 }
    }.map(\.heightM).sorted()
    let typical = heights.isEmpty ? 0 : heights[heights.count / 2]
    // Canyons need a steep camera or their floors (and the music) hide.
    let pitch = typical > 30 ? 14.0 : typical > 18 ? 36.0 : 50.0
    let view = View3D(name: "music", distance: min(max(typical > 30 ? 640 : 520, reach * 4.2), 1100), pitch: pitch,
                      heading: heading)
    let ahead = bearingVector(heading)
    let back = view.distance * (typical > 30 ? 0 : 0.12)
    // A steep camera sits a little nearer You so You stays clear of the edge.
    let focus = typical > 30 ? middle + (me - middle) * 0.3 : middle
    let center = (east: focus.x - ahead.east * back, north: focus.y - ahead.north * back)
    print(String(format: "music camera: median building %.0f m, pitch %.0f°, %.0f m out; song %.1f-%.1f s",
                 typical, pitch, view.distance, start, start + seconds))
    let stage = try Stage(snapshot(view, center: center, muted: true), view: view, center: center, hello: hello,
                          frame: frame)
    stage.musicLayers = musicLayers
    stage.musicTemperament = temperament
    stage.night = night
    let frames = output.appendingPathComponent("frames")
    try? FileManager.default.removeItem(at: frames)
    try FileManager.default.createDirectory(at: frames, withIntermediateDirectories: true)
    let count = Int(seconds * fps)
    var loudest = (index: 0, low: -200.0)
    for index in 0..<count {
        try autoreleasepool {
            let time = start + Double(index) / fps
            store.frozenMusicTime = time
            let low = music.levels.bands(at: time).x
            if low > loudest.low, index > 30 { loudest = (index, low) }
            guard let map = stage.compose(store: store, time: time) else { throw CocoaError(.fileWriteUnknown) }
            try write(store: store, map: map, to: frames.appendingPathComponent(String(format: "frame-%04d.png", index + 1)),
                      scale: 1)
        }
    }
    print("wrote \(count) music frames to \(frames.path); field \(music.counts), walls facing \(stage.musicScreen?.facingWalls ?? 0)")
    // Stills: a kick going out, and a quiet moment.
    let kick = track.track.kicksS.first { $0 > start + 4 } ?? start + 4
    for (name, time) in [("music-1-kick", kick + 0.12), ("music-2-ring-out", kick + 0.42),
                         ("music-3-loudest", start + Double(loudest.index) / fps)] {
        try autoreleasepool {
            store.frozenMusicTime = time
            guard let map = stage.compose(store: store, time: time) else { throw CocoaError(.fileWriteUnknown) }
            let url = output.appendingPathComponent("\(name).png")
            try write(store: store, map: map, to: url, scale: stage.scale)
            print("wrote \(url.path) at song \(String(format: "%.2f", time)) s")
        }
    }
}

MainActor.assumeIsolated {
    do {
        if musicMode {
            try renderMusic()
        } else if pulseMode {
            try renderPulse()
        } else if let recordedShot {
            try renderShot(recordedShot)
        } else {
            try renderViews()
        }
    } catch {
        FileHandle.standardError.write(Data("snapshot failed: \(error.localizedDescription)\n".utf8))
        exit(1)
    }
}
