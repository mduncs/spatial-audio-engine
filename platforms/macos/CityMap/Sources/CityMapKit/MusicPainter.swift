import CoreGraphics
import Foundation
import simd

// Music on the map: a continuous source gets a continuous field, not rings.
// Every walkable dot lights with the song's level there: the Workbench routes
// the speaker to each dot (spreading, air per band and a band loss at every
// corner), and the song's own three bands at the playhead, delayed by the
// routed distance and eased through an envelope, make the streets breathe.
// Music has its own calm palette (impact fire belongs to shots): bass fills
// and wraps corners, mids are warm, highs twinkle only near the speaker and
// in line of sight. Small grains drift along the real paths to You, a thin
// ripple runs out on every kick, and walls that return strong reflections
// glow softly in time. A temperament sets how much of each.

/// Which parts of the music look to draw; the live map draws `.standard`.
public struct MusicLayers: OptionSet, Sendable {
    public let rawValue: Int
    public init(rawValue: Int) { self.rawValue = rawValue }
    /// Bass and mid heat on the walkable dots.
    public static let field = MusicLayers(rawValue: 1)
    public static let highs = MusicLayers(rawValue: 2)
    public static let kicks = MusicLayers(rawValue: 4)
    public static let flow = MusicLayers(rawValue: 8)
    public static let bloom = MusicLayers(rawValue: 16)
    public static let walls = MusicLayers(rawValue: 32)
    /// Each dot as a column as tall as its level.
    public static let columns = MusicLayers(rawValue: 64)
    /// Where each path arrives at You, as wedges with their route lengths.
    public static let earCone = MusicLayers(rawValue: 128)
    public static let standard: MusicLayers = [.field, .highs, .kicks, .flow, .bloom, .walls]
}

/// How music behaves on the map: its own palette (impact fire stays with
/// shots), how slowly the field breathes and how loud each layer may get.
/// The structure is the same in every temperament: field, flow, kicks,
/// walls. The live map uses `.groove`.
public struct MusicTemperament: Sendable {
    public let name: String
    public let bass: RGB, mid: RGB, high: RGB, kick: RGB, wall: RGB
    /// Envelope on the song's bands (seconds): how fast the field swells
    /// and lets go.
    public let attack: Double, release: Double
    /// How much of the song's band swing reaches the field (0...1).
    public let swing: Double
    /// Peak alpha of the bass and mid field.
    public let bassAlpha: Double, midAlpha: Double
    /// Highs: how many line-of-sight dots twinkle, how bright, how fast.
    public let highDensity: Double, highAlpha: Double, twinkleHz: Double
    /// Kicks: ripple width (m, one sigma), peak alpha and fade (s).
    public let kickWidth: Double, kickAlpha: Double, kickFade: Double
    /// Flow: grain speed (m/s), spacing multiplier, size multiplier,
    /// grain alpha and its bright core.
    public let flowSpeed: Double, flowGap: Double, flowSize: Double, flowAlpha: Double, flowCore: Double
    /// The speaker's own glow, and how much it swells.
    public let bloomAlpha: Double, bloomPunch: Double
    /// Facade glow peak and the base stroke.
    public let wallAlpha: Double, wallStroke: CGFloat

    /// A: slow tide, indigo and peach; the streets just breathe.
    public static let calm = MusicTemperament(
        name: "calm",
        bass: RGB(0.40, 0.44, 0.80), mid: RGB(0.90, 0.72, 0.60), high: RGB(0.95, 0.94, 0.88),
        kick: RGB(0.78, 0.80, 0.96), wall: RGB(0.72, 0.68, 0.88),
        attack: 0.25, release: 0.6, swing: 0.6,
        bassAlpha: 0.2, midAlpha: 0.18,
        highDensity: 0.25, highAlpha: 0.3, twinkleHz: 0.35,
        kickWidth: 2.6, kickAlpha: 0.16, kickFade: 0.5,
        flowSpeed: 9, flowGap: 3.2, flowSize: 0.5, flowAlpha: 0.35, flowCore: 0,
        bloomAlpha: 0.28, bloomPunch: 0.1,
        wallAlpha: 0.3, wallStroke: 1)

    /// B: the default. Soft teal bass, amber mids, pale gold highs; a
    /// breathing field, a thin ripple on each kick.
    public static let groove = MusicTemperament(
        name: "groove",
        bass: RGB(0.34, 0.60, 0.66), mid: RGB(0.96, 0.72, 0.48), high: RGB(1.0, 0.93, 0.76),
        kick: RGB(0.76, 0.90, 0.94), wall: RGB(0.94, 0.76, 0.56),
        attack: 0.15, release: 0.3, swing: 0.85,
        bassAlpha: 0.26, midAlpha: 0.3,
        highDensity: 0.4, highAlpha: 0.5, twinkleHz: 0.7,
        kickWidth: 3, kickAlpha: 0.35, kickFade: 0.7,
        flowSpeed: 16, flowGap: 2.2, flowSize: 0.6, flowAlpha: 0.55, flowCore: 0.3,
        bloomAlpha: 0.35, bloomPunch: 0.2,
        wallAlpha: 0.5, wallStroke: 1.4)

    /// C: the earlier look tamed. Violet bass, amber mids, gold sparks;
    /// faster grains and clearer rings, but no fire and no strobe.
    public static let party = MusicTemperament(
        name: "party",
        bass: RGB(0.62, 0.36, 0.88), mid: RGB(1.0, 0.70, 0.36), high: RGB(1.0, 0.94, 0.72),
        kick: RGB(0.92, 0.80, 1.0), wall: RGB(0.86, 0.60, 0.92),
        attack: 0.08, release: 0.2, swing: 1.0,
        bassAlpha: 0.32, midAlpha: 0.3,
        highDensity: 0.6, highAlpha: 0.7, twinkleHz: 1.6,
        kickWidth: 4, kickAlpha: 0.5, kickFade: 0.9,
        flowSpeed: 28, flowGap: 1.5, flowSize: 0.8, flowAlpha: 0.7, flowCore: 0.55,
        bloomAlpha: 0.5, bloomPunch: 0.35,
        wallAlpha: 0.75, wallStroke: 2)

    public static let all: [MusicTemperament] = [.calm, .groove, .party]

    public static func named(_ name: String) -> MusicTemperament? {
        all.first { $0.name == name.lowercased() }
    }
}

/// The live map's music colours (the legend uses these too).
public enum MusicPalette {
    public static var bass: RGB { MusicTemperament.groove.bass }
    public static var mid: RGB { MusicTemperament.groove.mid }
    public static var high: RGB { MusicTemperament.groove.high }
    public static var kick: RGB { MusicTemperament.groove.kick }
    public static var wall: RGB { MusicTemperament.groove.wall }
}

/// A song's three bands over time: its own track when the speaker plays a
/// song, otherwise the Workbench's output meter with a typical music tilt.
public struct MusicLevels: Sendable {
    let track: SongTrack.Body?
    /// Loud-average level per band (dBFS) and their energy sum.
    let means: SIMD3<Double>
    let total: Double

    init(track: SongTrack.Body?, means: SIMD3<Double>, total: Double) {
        self.track = track
        self.means = means
        self.total = total
    }

    /// The same song through an envelope: each band rises with time
    /// constant `attack` and falls with `release` (seconds), so the field
    /// swells and settles instead of strobing. Run around the loop twice
    /// so the start already carries the end's state.
    public func smoothed(attack: Double, release: Double) -> MusicLevels {
        guard let track, track.bandsDb.count >= 6, track.rateHz > 0 else { return self }
        let frames = track.bandsDb.count / 3
        let rise = exp(-1 / (track.rateHz * max(attack, 1e-3)))
        let fall = exp(-1 / (track.rateHz * max(release, 1e-3)))
        var state = SIMD3(track.bandsDb[(frames - 1) * 3], track.bandsDb[(frames - 1) * 3 + 1],
                          track.bandsDb[(frames - 1) * 3 + 2])
        var out = track.bandsDb
        for pass in 0..<2 {
            for frame in 0..<frames {
                for band in 0..<3 {
                    let x = track.bandsDb[frame * 3 + band]
                    let k = x > state[band] ? rise : fall
                    state[band] = x + (state[band] - x) * k
                    if pass == 1 { out[frame * 3 + band] = state[band] }
                }
            }
        }
        let body = SongTrack.Body(rateHz: track.rateHz, lengthS: track.lengthS, bandsDb: out, kicksS: track.kicksS)
        return MusicLevels(track: body, means: means, total: total)
    }

    public init(track: SongTrack.Body?) {
        self.track = track
        if let track, track.bandsDb.count >= 3 {
            var sums = SIMD3<Double>(0, 0, 0)
            let frames = track.bandsDb.count / 3
            for frame in 0..<frames {
                for band in 0..<3 { sums[band] += pow(10, track.bandsDb[frame * 3 + band] / 10) }
            }
            let means = SIMD3<Double>(10 * log10(sums[0] / Double(frames) + 1e-12),
                                      10 * log10(sums[1] / Double(frames) + 1e-12),
                                      10 * log10(sums[2] / Double(frames) + 1e-12))
            self.means = means
        } else {
            means = SIMD3(-18, -21, -28)
        }
        total = 10 * log10(pow(10, means[0] / 10) + pow(10, means[1] / 10) + pow(10, means[2] / 10))
    }

    /// Band level at song time `t` relative to the song's overall average
    /// (dB). `meter` (0...1) stands in when there is no track.
    public func bands(at t: Double, meter: Double = 0.6) -> SIMD3<Double> {
        guard let track, track.bandsDb.count >= 6, track.lengthS > 0 else {
            let swing = 30 * (meter - 0.6)
            return means - total + SIMD3(repeating: swing)
        }
        let frames = track.bandsDb.count / 3
        var position = t.truncatingRemainder(dividingBy: track.lengthS)
        if position < 0 { position += track.lengthS }
        let x = position * track.rateHz
        let a = min(Int(x), frames - 1), b = min(a + 1, frames - 1)
        let f = x - Double(a)
        var out = SIMD3<Double>(0, 0, 0)
        for band in 0..<3 {
            let first: Double = track.bandsDb[a * 3 + band]
            let second: Double = track.bandsDb[b * 3 + band]
            out[band] = first * (1 - f) + second * f - total
        }
        return out
    }

    /// Kicks in `(t - window, t]`, as seconds since each.
    public func kicksAgo(at t: Double, window: Double) -> [Double] {
        guard let track, !track.kicksS.isEmpty, track.lengthS > 0 else { return [] }
        var position = t.truncatingRemainder(dividingBy: track.lengthS)
        if position < 0 { position += track.lengthS }
        var out: [Double] = []
        for kick in track.kicksS {
            var ago = position - kick
            if ago < 0 { ago += track.lengthS }
            if ago >= 0, ago < window { out.append(ago) }
        }
        return out
    }
}

/// One music source's field, paths and walls, ready to draw.
public final class MusicPlayback: @unchecked Sendable {
    public let id: String
    public let source: SIMD2<Double>
    public let sourceUp: Double
    public var splDb: Double
    public let levels: MusicLevels
    /// The song through each temperament's envelope, by name.
    let breathing: [String: MusicLevels]
    let dots: [SIMD2<Double>]
    let bandDb: [SIMD3<Double>]
    let routeM: [Double]
    let lineOfSight: [Bool]
    let walls: [Wall]
    let buildings: [Hello.Building]
    private(set) var paths: [FlowPath] = []
    let spacing: Double

    struct Wall {
        let a: SIMD2<Double>, b: SIMD2<Double>
        let height: Double
        let ringSign: Double
        let dot: Int
        let facing: Double
    }

    struct FlowPath {
        let points: [SIMD2<Double>]
        let cum: [Double]
        let turnAt: [Double]
        /// Level per band at You (dB re 1 m), and the residual loss (air,
        /// wall) spread along the path so the end matches it.
        let residual: SIMD3<Double>
        let echo: Bool
        let wall: SIMD2<Double>?
        var length: Double { cum.last ?? 0 }

        init?(_ path: MusicPaths.Path, echo: Bool) {
            let points = path.points.map(ground)
            guard points.count >= 2 else { return nil }
            var cum = [0.0], turns = [0.0]
            var turned = 0.0
            for index in points.indices.dropFirst() {
                cum.append(cum[index - 1] + simd_length(points[index] - points[index - 1]))
            }
            for index in points.indices.dropFirst().dropLast() {
                let a = points[index] - points[index - 1], b = points[index + 1] - points[index]
                if simd_length(a) > 1e-3, simd_length(b) > 1e-3 {
                    turned += acos(min(max(simd_dot(simd_normalize(a), simd_normalize(b)), -1), 1))
                }
                turns.append(turned)
            }
            turns.append(turned)
            let length = max(cum.last ?? 1, 1)
            let end = SIMD3(path.bandDb[0], path.bandDb[1], path.bandDb[2])
            let modelled = SIMD3<Double>(repeating: -20 * log10(length)) - 8.686 * MusicPlayback.cornerLoss * turned
            residual = end - modelled
            self.points = points
            self.cum = cum
            turnAt = turns
            self.echo = echo
            wall = path.wallM.map(ground)
        }

        /// Band level (dB re 1 m) a distance `s` along the path.
        func level(at s: Double) -> SIMD3<Double> {
            var turned = 0.0
            for index in cum.indices where cum[index] <= s { turned = turnAt[index] }
            let spreading = -20 * log10(max(s, 1))
            return SIMD3(repeating: spreading) - 8.686 * MusicPlayback.cornerLoss * turned
                + residual * (s / max(length, 1))
        }

        func position(at s: Double) -> (SIMD2<Double>, SIMD2<Double>) {
            var index = 0
            while index < cum.count - 2 && cum[index + 1] < s { index += 1 }
            let a = points[index], b = points[index + 1]
            let span = max(cum[index + 1] - cum[index], 1e-6)
            let f = min(max((s - cum[index]) / span, 0), 1)
            let along = simd_length(b - a) > 1e-6 ? simd_normalize(b - a) : SIMD2(1, 0)
            return (a + (b - a) * f, SIMD2(-along.y, along.x))
        }
    }

    /// Same as the Workbench's `CORNER_LOSS_PER_RADIAN`.
    static let cornerLoss = SIMD3<Double>(0.22, 0.85, 1.9)

    public init(field: Field, hello: Hello, splDb: Double, track: SongTrack.Body?) {
        id = field.id
        source = ground(field.sourceM)
        sourceUp = field.sourceM.count > 2 ? field.sourceM[2] : 3
        self.splDb = splDb
        let levels = MusicLevels(track: track)
        self.levels = levels
        breathing = Dictionary(uniqueKeysWithValues: MusicTemperament.all.map {
            ($0.name, levels.smoothed(attack: $0.attack, release: $0.release))
        })
        spacing = hello.dots.spacingM
        var dots: [SIMD2<Double>] = [], bands: [SIMD3<Double>] = [], routes: [Double] = [], los: [Bool] = []
        for dot in field.dots where dot.count >= 6 {
            let p = SIMD2(dot[0], dot[1])
            dots.append(p)
            bands.append(SIMD3(dot[2], dot[3], dot[4]))
            routes.append(dot[5])
            los.append(dot[5] <= simd_length(p - ground(field.sourceM)) * 1.01 + 0.5)
        }
        self.dots = dots
        bandDb = bands
        routeM = routes
        lineOfSight = los

        // Walls: footprint edges near the speaker, lit by the dot just
        // outside them.
        var cells: [SIMD2<Int32>: Int] = [:]
        let cell = max(spacing, 2)
        for (index, p) in dots.enumerated() {
            cells[SIMD2(Int32((p.x / cell).rounded(.down)), Int32((p.y / cell).rounded(.down)))] = index
        }
        func nearestDot(_ q: SIMD2<Double>) -> Int? {
            let cx = Int32((q.x / cell).rounded(.down)), cy = Int32((q.y / cell).rounded(.down))
            var best: (Int, Double)?
            for dx in -1...1 {
                for dy in -1...1 {
                    guard let index = cells[SIMD2(cx + Int32(dx), cy + Int32(dy))] else { continue }
                    let d = simd_length(dots[index] - q)
                    if d < 1.6 * cell, best.map({ d < $0.1 }) ?? true { best = (index, d) }
                }
            }
            return best?.0
        }
        let origin = ground(field.sourceM)
        let reach = (dots.map { simd_length($0 - origin) }.max() ?? 300) + 20
        let near = (hello.buildings ?? []).filter { building in
            building.ring.contains { simd_length(ground($0) - ground(field.sourceM)) < reach }
        }
        buildings = near
        var walls: [Wall] = []
        for building in near {
            let ring = building.ring.map(ground)
            guard ring.count >= 3 else { continue }
            var area = 0.0
            for index in ring.indices {
                let a = ring[index], b = ring[(index + 1) % ring.count]
                area += a.x * b.y - b.x * a.y
            }
            let sign: Double = area >= 0 ? 1 : -1
            for index in ring.indices {
                let a = ring[index], b = ring[(index + 1) % ring.count]
                let edge = b - a
                guard simd_length(edge) > 3 else { continue }
                // Outward: right of the edge for a counter-clockwise ring.
                let outward = simd_normalize(SIMD2(edge.y, -edge.x)) * sign
                let middle = (a + b) / 2
                guard let dot = nearestDot(middle + outward * 4) else { continue }
                let toSource = origin - middle
                let facing = simd_length(toSource) > 1e-6 ? max(0, simd_dot(outward, simd_normalize(toSource))) : 0
                walls.append(Wall(a: a, b: b, height: building.heightM, ringSign: sign, dot: dot, facing: facing))
            }
        }
        self.walls = walls
    }

    /// Walls lit by the field and paths to You (for render logs).
    public var counts: (dots: Int, walls: Int, paths: Int) { (dots.count, walls.count, paths.count) }

    public func setPaths(_ paths: MusicPaths?) {
        guard let paths else {
            self.paths = []
            return
        }
        self.paths = ([FlowPath(paths.primary, echo: false)] + paths.echoes.map { FlowPath($0, echo: true) })
            .compactMap { $0 }
    }

    /// 0...1 brightness for a band level in dB SPL.
    static func bright(_ level: Double, floor: Double, span: Double) -> Double {
        min(max((level - floor) / span, 0), 1)
    }

    /// Band levels (dB SPL) at dot `i` at song time `t`, the song delayed
    /// by the routed distance.
    func level(_ i: Int, at t: Double, meter: Double) -> SIMD3<Double> {
        SIMD3(repeating: splDb) + bandDb[i] + levels.bands(at: t - routeM[i] / 343, meter: meter)
    }

    /// The song as `temperament` hears it: enveloped.
    func breathing(_ temperament: MusicTemperament) -> MusicLevels {
        breathing[temperament.name] ?? levels.smoothed(attack: temperament.attack, release: temperament.release)
    }

    /// `level(_:at:meter:)` through a temperament's envelope and swing.
    func level(_ i: Int, at t: Double, meter: Double, song: MusicLevels, swing: Double) -> SIMD3<Double> {
        SIMD3(repeating: splDb) + bandDb[i] + song.bands(at: t - routeM[i] / 343, meter: meter) * swing
    }
}

/// The field's dots and walls in one camera's view; rebuild on camera moves.
public struct MusicScreen {
    let points: [CGPoint?]
    let pointsPerMetre: CGFloat
    let walls: [(quad: [CGPoint], index: Int)]
    let silhouettes: CGPath
    public var facingWalls: Int { walls.count }

    public init(_ music: MusicPlayback, projector: MapProjector, bounds: CGRect) {
        let visible = bounds.insetBy(dx: -60, dy: -60)
        points = music.dots.map { dot in
            projector.point(east: dot.x, north: dot.y).flatMap { visible.contains($0) ? $0 : nil }
        }
        pointsPerMetre = projector.pointsPerMetre(east: music.source.x, north: music.source.y)
        var walls: [(quad: [CGPoint], index: Int)] = []
        for (index, wall) in music.walls.enumerated() {
            guard let a = projector.point(east: wall.a.x, north: wall.a.y),
                  let b = projector.point(east: wall.b.x, north: wall.b.y) else { continue }
            let top = min(wall.height, 60)
            guard let topA = projector.point(east: wall.a.x, north: wall.a.y, up: top),
                  let topB = projector.point(east: wall.b.x, north: wall.b.y, up: top) else { continue }
            let quad = [a, b, topB, topA]
            guard quad.contains(where: { visible.contains($0) }) else { continue }
            var area: CGFloat = 0
            for i in quad.indices {
                let p = quad[i], q = quad[(i + 1) % quad.count]
                area += p.x * q.y - q.x * p.y
            }
            // Facing the camera (y-down screen winding against the ring's),
            // and seen as a face rather than an edge-on sliver.
            let face = abs(area) / 2 / max(hypot(b.x - a.x, b.y - a.y), 1)
            if Double(area) * wall.ringSign < 0, face >= 4 { walls.append((quad, index)) }
        }
        self.walls = walls
        silhouettes = Silhouettes.path(music.buildings, projector: projector, bounds: bounds)
    }
}

public enum MusicPainter {
    /// Splat sprites for every named temperament, keyed "temperament.band".
    private static let sprites: [String: CGImage] = {
        var out: [String: CGImage] = [:]
        for look in MusicTemperament.all {
            for (name, color) in [("bass", look.bass), ("mid", look.mid), ("high", look.high), ("kick", look.kick)] {
                out["\(look.name).\(name)"] = sprite(color)
            }
        }
        return out
    }()

    private static func sprite(_ color: RGB) -> CGImage? {
        let size = 64
        guard let context = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                                      space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue),
              let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
                                        colors: [color.mixed(with: .white, 0.12).cg(0.9), color.cg(0.5),
                                                 color.cg(0.15), color.cg(0)] as CFArray,
                                        locations: [0, 0.25, 0.6, 1])
        else { return nil }
        let middle = CGPoint(x: size / 2, y: size / 2)
        context.drawRadialGradient(gradient, startCenter: middle, startRadius: 0, endCenter: middle,
                                   endRadius: CGFloat(size) / 2, options: [])
        return context.makeImage()
    }

    private static func splat(_ context: CGContext, _ look: MusicTemperament, _ name: String, at point: CGPoint,
                              radius: CGFloat, alpha: Double) {
        guard alpha > 0.008,
              let image = sprites["\(look.name).\(name)"] ?? sprites["\(MusicTemperament.groove.name).\(name)"]
        else { return }
        context.setAlpha(CGFloat(min(alpha, 1)))
        context.draw(image, in: CGRect(x: point.x - radius, y: point.y - radius, width: radius * 2, height: radius * 2))
    }

    static func hash(_ a: Int, _ b: Int) -> Double {
        var value = UInt64(truncatingIfNeeded: a) &* 0x9E37_79B9_7F4A_7C15 ^ UInt64(truncatingIfNeeded: b) &* 0xC2B2_AE3D_27D4_EB4F
        value ^= value >> 31
        value = value &* 0xBF58_476D_1CE4_E5B9
        value ^= value >> 29
        return Double(value >> 11) / Double(1 << 53)
    }

    /// 0...1 as `value` passes `threshold` over `soft`, so things fade in
    /// and out instead of popping.
    static func ease(_ value: Double, over threshold: Double, soft: Double) -> Double {
        min(max((value - threshold) / soft, 0), 1)
    }

    /// The living field, kick ripples, streams to You and the speaker's
    /// glow, on the ground under the buildings; then softly lit walls on
    /// top. `t` is the song's playhead in seconds; `meter` the output level
    /// 0...1. Everything follows the song through the temperament's
    /// envelope, so nothing strobes.
    public static func draw(_ context: CGContext, _ music: MusicPlayback, screen: MusicScreen,
                            projector: MapProjector, time t: Double, meter: Double = 0.6,
                            layers: MusicLayers = .standard, temperament look: MusicTemperament = .groove) {
        let song = music.breathing(look)
        let ppm = max(screen.pointsPerMetre, 0.05)
        let step = CGFloat(music.spacing) * ppm
        let kicks = music.levels.kicksAgo(at: t, window: look.kickFade)
        var levels = [SIMD3<Double>](repeating: SIMD3(0, 0, 0), count: music.dots.count)

        context.saveGState()
        context.beginTransparencyLayer(auxiliaryInfo: nil)
        context.setBlendMode(.plusLighter)
        for i in music.dots.indices {
            guard let point = screen.points[i] else { continue }
            let level = music.level(i, at: t, meter: meter, song: song, swing: look.swing)
            levels[i] = level
            let bass = MusicPlayback.bright(level[0], floor: 54, span: 42)
            let mid = MusicPlayback.bright(level[1], floor: 60, span: 34)
            if layers.contains(.field) {
                splat(context, look, "bass", at: point, radius: max(step * 3, 8), alpha: look.bassAlpha * pow(bass, 1.1))
                splat(context, look, "mid", at: point, radius: max(step * 1.3, 3.5), alpha: look.midAlpha * pow(mid, 1.5))
            }
            if layers.contains(.columns), bass > 0.04 {
                let dot = music.dots[i]
                if let top = projector.point(east: dot.x, north: dot.y, up: 3 + 42 * pow(bass, 1.4)) {
                    let color = look.bass.mixed(with: look.mid, mid).mixed(with: .white, 0.1)
                    context.setStrokeColor(color.cg(CGFloat(0.2 + 0.4 * bass)))
                    context.setLineWidth(max(step * 0.28, 1.4))
                    context.setLineCap(.round)
                    context.move(to: point)
                    context.addLine(to: top)
                    context.strokePath()
                    splat(context, look, "mid", at: top, radius: max(step * 0.7, 2.5), alpha: 0.35 * bass)
                }
            }
            // Highs: a slow twinkle, only in line of sight and near. Each
            // dot keeps its own phase and rate; none flicker per frame.
            if layers.contains(.highs), music.lineOfSight[i] {
                let high = MusicPlayback.bright(level[2], floor: 74, span: 24)
                let share = ease(look.highDensity * high, over: hash(i, 7) * 0.9, soft: 0.08)
                if share > 0 {
                    let rate = look.twinkleHz * (0.7 + 0.6 * hash(i, 11))
                    let twinkle = 0.5 + 0.5 * sin(2 * .pi * (rate * t + hash(i, 13)))
                    splat(context, look, "high", at: point, radius: max(step * (0.4 + 0.35 * high), 2.2),
                          alpha: look.highAlpha * high * share * (0.25 + 0.75 * twinkle * twinkle))
                }
            }
            // Each kick: one thin ripple out through the streets at its
            // routed range, gone well before the next.
            for ago in kicks where layers.contains(.kicks) {
                let x = music.routeM[i] - 343 * ago
                let ring = exp(-(x * x) / (2 * look.kickWidth * look.kickWidth))
                guard ring > 0.04 else { continue }
                let reach = MusicPlayback.bright(level[0] + 10, floor: 52, span: 40)
                let fade = 1 - ago / look.kickFade
                splat(context, look, "kick", at: point, radius: max(step * 1.4, 4.5),
                      alpha: look.kickAlpha * ring * reach * fade * fade)
            }
        }
        // Streams to You along the real routed and echo paths: small, sparse
        // grains that ease in and out rather than pop.
        for (k, path) in music.paths.enumerated() where layers.contains(.flow) {
            let bands: [(String, Double, Double, Double, Double)] = [
                ("bass", 2.6, 54, 40, 1.0), ("mid", 3.6, 58, 34, 0.85), ("high", 5.0, 70, 26, 0.7),
            ]
            for (b, band) in bands.enumerated() {
                let (name, gapBase, floor, span, size) = band
                let gap = gapBase * look.flowGap
                let count = Int(path.length / gap)
                guard count > 0 else { continue }
                for j in 0..<count {
                    let s = (Double(j) * gap + look.flowSpeed * t + hash(k, j * 7 + b) * gap)
                        .truncatingRemainder(dividingBy: path.length)
                    let level = SIMD3(repeating: music.splDb) + path.level(at: s)
                        + song.bands(at: t - s / 343, meter: meter) * look.swing
                    let bright = MusicPlayback.bright(level[b], floor: floor, span: span)
                    let presence = ease(0.25 + 0.85 * bright, over: hash(j, b * 31 + k), soft: 0.12)
                        * min(s / 8, 1) * min((path.length - s) / 8, 1)
                    guard bright > 0.04, presence > 0 else { continue }
                    let (p, across) = path.position(at: s)
                    let wobble = (hash(j, k * 13 + b) - 0.5) * 3.2 + 0.5 * sin(t * 1.1 + Double(j))
                    let q = p + across * wobble
                    guard let point = projector.point(east: q.x, north: q.y) else { continue }
                    let radius = max(CGFloat(size * look.flowSize) * 1.6 * ppm, 1.6) * (0.8 + 0.6 * CGFloat(bright))
                    let echo = path.echo ? 0.6 : 1
                    splat(context, look, name, at: point, radius: radius * 1.8,
                          alpha: echo * look.flowAlpha * bright * presence)
                    if look.flowCore > 0 {
                        splat(context, look, "high", at: point, radius: radius * 0.6,
                              alpha: echo * look.flowCore * pow(bright, 0.8) * presence)
                    }
                }
            }
        }
        // The speaker glows, swelling a little on the low end.
        if layers.contains(.bloom), let middle = projector.point(east: music.source.x, north: music.source.y) {
            let low = song.bands(at: t, meter: meter)[0] * look.swing
            let punch = min(max((low + 12) / 14, 0), 1)
            let alpha = look.bloomAlpha + look.bloomPunch * punch
            splat(context, look, "bass", at: middle, radius: max(26 * ppm, 30) * CGFloat(0.85 + look.bloomPunch * punch),
                  alpha: alpha)
            splat(context, look, "mid", at: middle, radius: max(9 * ppm, 12), alpha: alpha * 1.1)
        }
        context.setAlpha(1)
        context.setBlendMode(.normal)
        Silhouettes.cut(context, screen.silhouettes)
        context.endTransparencyLayer()
        context.restoreGState()

        // Walls in time: the level just outside each face, stronger where
        // it faces the speaker and where an echo to You bounces; a soft
        // pulse that follows the envelope, never a flash.
        context.saveGState()
        context.setBlendMode(.plusLighter)
        let echoWalls = music.paths.compactMap(\.wall)
        for (quad, index) in screen.walls where layers.contains(.walls) {
            let wall = music.walls[index]
            let level = levels[wall.dot] == SIMD3(0, 0, 0)
                ? music.level(wall.dot, at: t, meter: meter, song: song, swing: look.swing) : levels[wall.dot]
            var glow = MusicPlayback.bright(max(level[0] - 2, level[1] + 2), floor: 57, span: 32)
                * (0.35 + 0.65 * wall.facing)
            let middle = (wall.a + wall.b) / 2
            if echoWalls.contains(where: { simd_length($0 - middle) < max(simd_length(wall.b - wall.a) / 2, 6) }) {
                glow = min(1, glow * 1.3 + 0.15)
            }
            let peak = glow * look.wallAlpha
            guard peak > 0.015 else { continue }
            let path = CGMutablePath()
            path.addLines(between: quad)
            path.closeSubpath()
            context.saveGState()
            context.addPath(path)
            context.clip()
            let colors = [look.wall.cg(CGFloat(peak)), look.wall.cg(CGFloat(0.4 * peak)),
                          look.wall.cg(CGFloat(0.05 * peak))] as CFArray
            if let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: colors,
                                         locations: [0, 0.45, 1]) {
                let base = CGPoint(x: (quad[0].x + quad[1].x) / 2, y: (quad[0].y + quad[1].y) / 2)
                let top = CGPoint(x: (quad[2].x + quad[3].x) / 2, y: (quad[2].y + quad[3].y) / 2)
                context.drawLinearGradient(gradient, start: base, end: top, options: [])
            }
            context.restoreGState()
            context.setStrokeColor(look.wall.mixed(with: .white, 0.2).cg(CGFloat(min(1.6 * peak, 0.9))))
            context.setLineWidth(look.wallStroke)
            context.move(to: quad[0])
            context.addLine(to: quad[1])
            context.strokePath()
        }
        context.restoreGState()
        if layers.contains(.earCone) {
            drawEarCone(context, music, projector: projector, time: t, meter: meter, look: look, song: song)
        }
    }

    /// Where the song reaches You from: a wedge down each path's last
    /// stretch, as bright as what arrives, with how far it travelled.
    static func drawEarCone(_ context: CGContext, _ music: MusicPlayback, projector: MapProjector, time t: Double,
                            meter: Double, look: MusicTemperament, song: MusicLevels) {
        guard let you = music.paths.first.flatMap({ $0.points.last }),
              let centre = projector.point(east: you.x, north: you.y) else { return }
        context.saveGState()
        context.setBlendMode(.plusLighter)
        for path in music.paths.sorted(by: { $0.echo && !$1.echo }) where path.length > 2 {
            let (back, _) = path.position(at: max(path.length - 8, 0))
            let from = simd_normalize(back - you)
            let level = SIMD3(repeating: music.splDb) + path.level(at: path.length)
                + song.bands(at: t - path.length / 343, meter: meter) * look.swing
            let bright = MusicPlayback.bright(max(level[0], level[1] + 4), floor: 50, span: 40)
            let length = (path.echo ? 50.0 : 80.0) * (0.6 + 0.4 * bright)
            let spread = (path.echo ? 8.0 : 15.0) * .pi / 180
            let color = path.echo ? look.wall : look.mid
            let wedge = CGMutablePath()
            wedge.move(to: centre)
            for step in 0...12 {
                let angle = -spread + 2 * spread * Double(step) / 12
                let direction = SIMD2(from.x * cos(angle) - from.y * sin(angle), from.x * sin(angle) + from.y * cos(angle))
                let tip = you + direction * length
                if let point = projector.point(east: tip.x, north: tip.y) { wedge.addLine(to: point) }
            }
            wedge.closeSubpath()
            context.saveGState()
            context.addPath(wedge)
            context.clip()
            let tipPoint = projector.point(east: you.x + from.x * length, north: you.y + from.y * length) ?? centre
            if let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
                                         colors: [color.cg(CGFloat(0.25 + 0.45 * bright)), color.cg(0.02)] as CFArray,
                                         locations: [0, 1]) {
                context.drawLinearGradient(gradient, start: centre, end: tipPoint, options: [])
            }
            context.restoreGState()
            context.addPath(wedge)
            context.setStrokeColor(color.mixed(with: .white, 0.4).cg(CGFloat(0.2 + 0.4 * bright)))
            context.setLineWidth(1.2)
            context.strokePath()
        }
        context.setBlendMode(.normal)
        // One label: how far the street route and each echo travelled.
        let street = music.paths.first { !$0.echo }
        let echoes = music.paths.filter(\.echo).map(\.length).sorted()
        var text = street.map { "Heard via the street · \(distanceText($0.length))" } ?? "Heard"
        if !echoes.isEmpty {
            text += "  ·  echoes " + echoes.prefix(4).map { String(format: "%.0f", $0) }.joined(separator: " / ") + " m"
        }
        let line = TextLine.line(text, size: 11.5, bold: true, color: CGColor(gray: 1, alpha: 0.94))
        let size = TextLine.size(line).size
        let box = CGRect(x: centre.x + 22, y: centre.y + 16, width: size.width + 16, height: size.height + 8)
        context.setFillColor(CGColor(gray: 0.05, alpha: 0.75))
        context.addPath(CGPath(roundedRect: box, cornerWidth: 6, cornerHeight: 6, transform: nil))
        context.fillPath()
        context.setFillColor(look.mid.cg(1))
        context.fill(CGRect(x: box.minX, y: box.minY + 4, width: 3, height: box.height - 8))
        TextLine.draw(context, line, at: CGPoint(x: box.minX + 9, y: box.minY + 4))
        context.restoreGState()
    }
}
