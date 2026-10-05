import CoreGraphics
import CoreText
import Foundation
import simd

// A shot on the map, drawn the way the 29 Sep "Fightbox Sound Visuals"
// mockup draws it, but from the engine's own acoustic-feed event: the crack
// band sweeps the ground first (the shell's real Mach cone), then impact
// rings run down each routed path the engine planned (not a shortest street
// guess), then echo rings bloom where each echo path turns off a facade, at
// that path's own timing. Ground light sits under 3D buildings: footprints
// projected at their heights cut it out, so it never shows through a block.

/// The mockup's dark palette.
public enum ShotPalette {
    public static let boom = RGB(1, 0.541, 0.361)
    public static let echo = RGB(0.655, 0.545, 0.98)
    public static let crack = RGB(1, 0.827, 0.302)
    public static let you = RGB(0.353, 0.69, 1)
    public static let edge = RGB(0.62, 0.67, 0.75)
    public static let card = RGB(0.09, 0.118, 0.169)
    public static let ink = RGB(0.906, 0.925, 0.953)
}

public enum ShotKind: String, Sendable {
    case crack, boom, echo

    public var color: RGB {
        switch self {
        case .crack: ShotPalette.crack
        case .boom: ShotPalette.boom
        case .echo: ShotPalette.echo
        }
    }
}

/// One arrival at you: the hit ring, a timeline mark and the readout.
public struct ShotMoment: Sendable {
    public let kind: ShotKind
    public let time: Double
    public let label: String
    /// Unit ground direction the sound comes from (the negated wave travel).
    public let from: SIMD2<Double>
}

typealias Ground = SIMD2<Double>

/// A path the sound runs along, timed by its own emission and arrival.
struct Track {
    let points: [Ground]
    let cum: [Double]
    let emission: Double
    let arrival: Double
    let prediction: Bool

    init(points: [Ground], emission: Double, arrival: Double, prediction: Bool) {
        self.points = points
        var cum = [0.0]
        for index in points.indices.dropFirst() {
            cum.append(cum[index - 1] + simd_length(points[index] - points[index - 1]))
        }
        self.cum = cum
        self.emission = emission
        self.arrival = arrival
        self.prediction = prediction
    }

    var length: Double { cum.last ?? 0 }
    /// Along-path speed; usually 343 m/s, a render-delay fallback otherwise.
    var speed: Double { arrival > emission + 1e-6 ? max(length, 1) / (arrival - emission) : 343 }

    /// Distance along the path of the nearest point, and how far off it `p` is.
    func project(_ p: Ground) -> (s: Double, lateral: Double) {
        var best = (s: 0.0, lateral: Double.infinity)
        for index in points.indices.dropLast() {
            let a = points[index], b = points[index + 1], ab = b - a
            let squared = simd_length_squared(ab)
            let t = squared > 0 ? min(max(simd_dot(p - a, ab) / squared, 0), 1) : 0
            let lateral = simd_length(p - (a + ab * t))
            if lateral < best.lateral { best = (cum[index] + t * sqrt(squared), lateral) }
        }
        return best
    }
}

/// An echo: the path up to its facade tap, then the leg back to you.
struct EchoTrack {
    let tap: Ground
    let tapTime: Double
    let leg: Track
    let label: String

    /// The tap is the sharpest turn off the main route (a facade bounce or
    /// the corner it wraps), so the bloom starts where the path really turns.
    init?(_ arrival: Shot.Arrival, primary: Track?) {
        let points = arrival.pathEnuM.map(ground)
        guard points.count >= 3 else { return nil }
        let whole = Track(points: points, emission: arrival.emissionTimeS, arrival: arrival.arrivalTimeS,
                          prediction: arrival.pathIsPrediction)
        let interior = Array(points.indices.dropFirst().dropLast())
        let off = interior.filter { index in primary.map { $0.project(points[index]).lateral > 4 } ?? true }
        func turn(_ index: Int) -> Double {
            let a = simd_normalize(points[index] - points[index - 1] + Ground(1e-9, 0))
            let b = simd_normalize(points[index + 1] - points[index] + Ground(1e-9, 0))
            return acos(min(max(simd_dot(a, b), -1), 1))
        }
        guard let tapIndex = (off.isEmpty ? interior : off).max(by: { turn($0) < turn($1) }) else { return nil }
        tap = points[tapIndex]
        tapTime = whole.emission + whole.cum[tapIndex] / whole.speed
        leg = Track(points: Array(points[tapIndex...]), emission: tapTime, arrival: arrival.arrivalTimeS,
                    prediction: arrival.pathIsPrediction)
        label = arrival.label
    }
}

/// The shell on its straight track, timed so it passes the crack's tangent
/// point at the crack's emission and lands at the boom's.
struct Shell {
    let impact: SIMD3<Double>
    let direction: SIMD3<Double>
    let speed: Double
    let impactTime: Double
    let span: Double

    init?(_ crack: Shot.Crack, impactTime: Double) {
        guard crack.flightTrackEnuM.count == 2, crack.tangentPositionEnuM.count >= 3 else { return nil }
        let muzzle = point3(crack.flightTrackEnuM[0]), impact = point3(crack.flightTrackEnuM[1])
        let track = impact - muzzle
        guard simd_length(track) > 1 else { return nil }
        direction = simd_normalize(track)
        let tangent = point3(crack.tangentPositionEnuM)
        let before = impactTime - crack.emissionTimeS
        let fallback = max(crack.mach, 1.01) * 343
        let timed = before > 0.01 ? simd_length(impact - tangent) / before : fallback
        speed = timed > 343 ? timed : fallback
        self.impact = impact
        self.impactTime = impactTime
        span = min(simd_length(track) / speed, 10)
    }

    func position(_ time: Double) -> SIMD3<Double> {
        impact - direction * speed * (impactTime - min(time, impactTime))
    }

    /// When the crack reaches a ground spot: the Mach-cone minimum of
    /// emission time plus travel. Nil outside the cone (minimum at impact).
    func crackTime(at p: Ground, height: Double = 1.5) -> Double? {
        let spot = SIMD3(p.x, p.y, height)
        func arrival(_ tau: Double) -> Double { tau + simd_length(spot - position(tau)) / 343 }
        var low = impactTime - span, high = impactTime
        for _ in 0..<48 {
            let a = low + (high - low) / 3, b = high - (high - low) / 3
            if arrival(a) < arrival(b) { high = b } else { low = a }
        }
        let tau = (low + high) / 2
        return tau < impactTime - 0.004 ? arrival(tau) : nil
    }
}

func ground(_ values: [Double]) -> Ground { Ground(values.first ?? 0, values.count > 1 ? values[1] : 0) }
func point3(_ values: [Double]) -> SIMD3<Double> {
    SIMD3(values.first ?? 0, values.count > 1 ? values[1] : 0, values.count > 2 ? values[2] : 0)
}

/// One received shot with everything precomputed for drawing it at any time.
public final class ShotPlayback: @unchecked Sendable {
    public let shot: Shot
    public let hello: Hello
    public let received: Date
    public let impact: SIMD2<Double>
    public let listener: SIMD2<Double>
    public let boomStart: Double
    public let moments: [ShotMoment]
    public let endTime: Double
    /// Straight-line and routed distance from the impact to you.
    public let straightM: Double
    public let routedM: Double?

    let routes: [Track]
    let echoes: [EchoTrack]
    let shell: Shell?
    let buildings: [Hello.Building]
    // Street samples every few metres, in runs along each street.
    let samples: [Ground]
    let widths: [Double]
    let runs: [Range<Int>]
    let toImpact: [Double]
    let crackAt: [Double]
    let routeAt: [[(s: Double, lateral: Double)]]
    let tapDistance: [[Double]]
    let legAt: [[(s: Double, lateral: Double)]]

    static let step = 3.0

    public init(shot: Shot, hello: Hello, received: Date) {
        self.shot = shot
        self.hello = hello
        self.received = received
        let event = shot.event
        impact = ground(event.sourcePositionEnuM)
        listener = ground(event.listenerPositionEnuM)
        boomStart = event.sourceEmissionTimeS
        straightM = simd_length(listener - impact)
        let routes = event.arrivals.filter { $0.kind == "routed_primary" || $0.kind == "direct" }.map {
            Track(points: $0.pathEnuM.map(ground), emission: $0.emissionTimeS, arrival: $0.arrivalTimeS,
                  prediction: $0.pathIsPrediction)
        }
        self.routes = routes
        routedM = event.arrivals.first { $0.kind == "routed_primary" || $0.kind == "direct" }?.lengthM
        echoes = event.arrivals.filter { $0.kind == "echo" }.compactMap { EchoTrack($0, primary: routes.first) }
        shell = event.crack.flatMap { Shell($0, impactTime: event.sourceEmissionTimeS) }
        moments = event.arrivals.map { arrival in
            let kind: ShotKind = arrival.kind == "crack" ? .crack : arrival.kind == "echo" ? .echo : .boom
            let travel = ground(arrival.arrivalDirectionEnu)
            let from = simd_length(travel) > 1e-6 ? -simd_normalize(travel) : Ground(0, 0)
            return ShotMoment(kind: kind, time: arrival.arrivalTimeS, label: kind.rawValue, from: from)
        }
        .sorted { $0.time < $1.time }
        endTime = moments.last?.time ?? event.sourceEmissionTimeS

        // Region: everything the sound runs along, plus room for the rings.
        var low = simd_min(impact, listener), high = simd_max(impact, listener)
        for arrival in event.arrivals {
            for point in arrival.pathEnuM.map(ground) where arrival.kind != "crack" {
                low = simd_min(low, point)
                high = simd_max(high, point)
            }
        }
        low -= Ground(160, 160)
        high += Ground(160, 160)
        func inside(_ p: Ground) -> Bool { p.x >= low.x && p.y >= low.y && p.x <= high.x && p.y <= high.y }
        buildings = (hello.buildings ?? []).filter { building in
            building.ring.contains { inside(ground($0)) }
        }

        var samples: [Ground] = [], widths: [Double] = [], runs: [Range<Int>] = []
        var lines = (hello.streets ?? []).map { ($0.points.map(ground), $0.widthM) }
        // The planned paths glow too, so the rings follow them even where
        // they cut between streets.
        lines += event.arrivals.filter { $0.kind != "crack" }.map { ($0.pathEnuM.map(ground), 7.0) }
        for (points, width) in lines where points.count >= 2 {
            var run: [Ground] = []
            func flush() {
                if run.count >= 2 {
                    runs.append(samples.count..<(samples.count + run.count))
                    samples += run
                    widths += Array(repeating: width, count: run.count)
                }
                run = []
            }
            for index in points.indices.dropLast() {
                let a = points[index], b = points[index + 1]
                let steps = max(1, Int((simd_length(b - a) / Self.step).rounded(.up)))
                for j in 0..<steps {
                    let q = a + (b - a) * (Double(j) / Double(steps))
                    if inside(q) { run.append(q) } else { flush() }
                }
            }
            if let last = points.last, inside(last) { run.append(last) }
            flush()
        }
        self.samples = samples
        self.widths = widths
        self.runs = runs
        toImpact = samples.map { simd_length($0 - ground(event.sourcePositionEnuM)) }
        let shell = self.shell
        crackAt = samples.map { shell?.crackTime(at: $0) ?? .nan }
        routeAt = routes.map { route in samples.map { route.project($0) } }
        tapDistance = echoes.map { echo in samples.map { simd_length($0 - echo.tap) } }
        legAt = echoes.map { echo in samples.map { echo.leg.project($0) } }
    }

    // MARK: the field

    static let lambda = 13.0

    /// The mockup's ring: a bright front with three fading trailing bands.
    static func ring(_ d: Double, _ r: Double, width: Double) -> Double {
        if r <= 0 || d > r + 3 * width { return 0 }
        var value = 0.0, amplitude = 1.0
        for n in 0..<4 {
            let x = d - (r - Double(n) * lambda)
            value += amplitude * exp(-(x * x) / (2 * width * width))
            amplitude *= 0.55
        }
        if d < r { value += 0.12 * exp(-(r - d) / 70) }
        return value
    }

    /// Boom, echo and crack strength at one street sample.
    func strength(_ i: Int, time t: Double, width w: Double) -> (boom: Double, echo: Double, crack: Double) {
        var boom = 0.0
        // The drop in the water: right at the impact, before streets take over.
        let near = 343 * (t - boomStart)
        if near > 0 { boom = Self.ring(toImpact[i], near, width: w) * exp(-toImpact[i] / 80) }
        for (k, route) in routes.enumerated() {
            let (s, lateral) = routeAt[k][i]
            let front = route.speed * (t - route.emission)
            guard lateral < 28, front > 0, s <= route.length + 45 else { continue }
            boom = max(boom, 1.15 * Self.ring(s, front, width: w) * exp(-lateral * lateral / 242) / (1 + s / 900))
        }
        var echo = 0.0
        for (k, track) in echoes.enumerated() {
            let front = track.leg.speed * (t - track.tapTime)
            guard front > 0 else { continue }
            let d = tapDistance[k][i]
            echo = max(echo, 0.8 * Self.ring(d, front, width: w) * exp(-d / 38))
            let (s, lateral) = legAt[k][i]
            if lateral < 24, s <= track.leg.length + 30 {
                echo = max(echo, 0.6 * Self.ring(s, front, width: w) * exp(-lateral * lateral / 128))
            }
        }
        var crack = 0.0
        let arrival = crackAt[i]
        if arrival.isFinite {
            let x = (t - arrival) * 343, scale = w / 2.6
            if x > -8 * scale && x < 40 * scale {
                crack = 0.8 * (x < 0 ? exp(-(x * x) / (8 * scale * scale)) : exp(-x / (7 * scale)))
            }
        }
        return (boom, echo, crack)
    }

    func color(_ i: Int, time: Double, width: Double) -> (RGB, Double)? {
        let (b, e, c) = strength(i, time: time, width: width)
        let sum = b + e + c
        guard sum > 0.015 else { return nil }
        let mix = { (x: KeyPath<RGB, CGFloat>) in
            (Double(ShotPalette.boom[keyPath: x]) * b + Double(ShotPalette.echo[keyPath: x]) * e
                + Double(ShotPalette.crack[keyPath: x]) * c) / sum
        }
        return (RGB(CGFloat(mix(\.red)), CGFloat(mix(\.green)), CGFloat(mix(\.blue))), min(1, sum) * 0.92)
    }

    /// The readout sentence under the timeline.
    public var readout: String {
        var parts: [String] = []
        let boom = moments.first { $0.kind == .boom }
        if let crack = moments.first(where: { $0.kind == .crack }) {
            parts.append(crack.time < (boom?.time ?? .infinity)
                ? String(format: "Crack at %.2f s, before the shell lands.", crack.time)
                : String(format: "Crack at %.2f s.", crack.time))
        } else if shot.event.crack == nil, shell == nil {
            parts.append("No crack here: you are outside the shell's Mach cone.")
        }
        if let boom {
            if shot.event.lineOfSight {
                parts.append(String(format: "Boom at %.2f s, straight down the street (%.0f m).", boom.time, straightM))
            } else if let routedM {
                parts.append(String(format: "Boom at %.2f s by the street route, %.0f m longer than the blocked straight line of %.0f m.",
                                    boom.time, max(0, routedM - straightM), straightM))
            }
        }
        let echoes = moments.filter { $0.kind == .echo }.map { String(format: "%.2f s", $0.time) }
        parts.append(echoes.isEmpty ? "No facade echo in range here." : "Echoes at \(echoes.joined(separator: ", ")).")
        return parts.joined(separator: " ")
    }
}

/// A shot's street samples and building outlines in one camera's view.
/// Rebuild when the camera moves; reuse across animation frames.
public struct ShotScreen {
    let points: [CGPoint?]
    let pointsPerMetre: [CGFloat]
    let typical: CGFloat
    let silhouettes: CGPath

    public init(_ playback: ShotPlayback, projector: MapProjector, bounds: CGRect) {
        let visible = bounds.insetBy(dx: -80, dy: -80)
        var bandScale: [Int: CGFloat] = [:]
        var points: [CGPoint?] = [], scales: [CGFloat] = []
        points.reserveCapacity(playback.samples.count)
        for sample in playback.samples {
            guard let point = projector.point(east: sample.x, north: sample.y), visible.contains(point) else {
                points.append(nil)
                scales.append(0)
                continue
            }
            let band = Int(point.y / 32)
            let scale = bandScale[band] ?? projector.pointsPerMetre(east: sample.x, north: sample.y)
            bandScale[band] = scale
            points.append(point)
            scales.append(scale)
        }
        self.points = points
        pointsPerMetre = scales
        typical = projector.pointsPerMetre(east: playback.impact.x, north: playback.impact.y)

        silhouettes = Silhouettes.path(playback.buildings, projector: projector, bounds: bounds)
    }
}

/// Buildings as screen outlines, for hiding ground marks behind them.
public enum Silhouettes {
    /// Each building as its projected prism: footprint, roof and walls, all
    /// wound the same way so one non-zero fill is their union. A ground spot
    /// inside it is behind (or under) a building from this camera.
    public static func path(_ buildings: [Hello.Building], projector: MapProjector, bounds: CGRect) -> CGPath {
        let visible = bounds.insetBy(dx: -80, dy: -80)
        let path = CGMutablePath()
        for building in buildings {
            let ring = building.ring.map(ground)
            let base = ring.compactMap { projector.point(east: $0.x, north: $0.y) }
            let roof = ring.compactMap { projector.point(east: $0.x, north: $0.y, up: building.heightM) }
            guard base.count == ring.count, roof.count == ring.count, base.count >= 3 else { continue }
            let box = (base + roof).reduce(CGRect.null) { $0.union(CGRect(origin: $1, size: .zero)) }
            guard box.intersects(visible) else { continue }
            func add(_ polygon: [CGPoint]) {
                var area: CGFloat = 0
                for index in polygon.indices {
                    let a = polygon[index], b = polygon[(index + 1) % polygon.count]
                    area += a.x * b.y - b.x * a.y
                }
                guard abs(area) > 0.01 else { return }
                path.addLines(between: area > 0 ? polygon : polygon.reversed())
                path.closeSubpath()
            }
            add(base)
            add(roof)
            for index in base.indices {
                let next = (index + 1) % base.count
                add([base[index], base[next], roof[next], roof[index]])
            }
        }
        return path
    }

    /// Clears whatever was drawn since `beginTransparencyLayer` under them.
    public static func cut(_ context: CGContext, _ path: CGPath) {
        context.saveGState()
        context.setBlendMode(.destinationOut)
        context.addPath(path)
        context.setFillColor(CGColor(gray: 0, alpha: 1))
        context.fillPath(using: .winding)
        context.restoreGState()
    }
}

public enum ShotPainter {
    /// Ring width in metres for this view, so the bands stay visible when
    /// the camera is pulled back.
    static func ringWidth(_ screen: ShotScreen) -> Double {
        max(2.6, 2.4 / Double(max(screen.typical, 0.05)))
    }

    /// Street light under the buildings: crack band, boom rings, echo blooms.
    public static func drawGround(_ context: CGContext, _ playback: ShotPlayback, screen: ShotScreen, time: Double) {
        let width = ringWidth(screen)
        context.saveGState()
        context.beginTransparencyLayer(auxiliaryInfo: nil)
        context.setLineCap(.round)
        for run in playback.runs {
            var previous: (CGPoint, RGB, Double)?
            for i in run {
                guard let point = screen.points[i], let (color, alpha) = playback.color(i, time: time, width: width)
                else {
                    previous = nil
                    continue
                }
                let stroke = max(2.5, CGFloat(playback.widths[i]) * screen.pointsPerMetre[i] * 0.8)
                if let (last, lastColor, lastAlpha) = previous {
                    context.setStrokeColor(lastColor.mixed(with: color, 0.5).cg(CGFloat((lastAlpha + alpha) / 2)))
                    context.setLineWidth(stroke)
                    context.move(to: last)
                    context.addLine(to: point)
                    context.strokePath()
                } else {
                    SoundPainter.disc(context, point, stroke / 2, fill: color.cg(CGFloat(alpha)))
                }
                previous = (point, color, alpha)
            }
        }
        // Buildings in front of the street hide it.
        Silhouettes.cut(context, screen.silhouettes)
        context.endTransparencyLayer()
        context.restoreGState()
    }

    /// Paths, shell, impact, facade taps and your hit rings, over everything.
    public static func drawTop(_ context: CGContext, _ playback: ShotPlayback, projector: MapProjector,
                               time t: Double) {
        func screen(_ p: Ground) -> CGPoint? { projector.point(east: p.x, north: p.y) }
        func lifted(_ p: SIMD3<Double>) -> CGPoint? {
            projector.point(east: p.x, north: p.y, up: p.z)
        }
        func polyline(_ points: [Ground]) -> CGPath? {
            let projected = points.compactMap(screen)
            guard projected.count >= 2 else { return nil }
            let path = CGMutablePath()
            path.addLines(between: projected)
            return path
        }
        func groundCircle(_ centre: Ground, radius: Double, minimum: CGFloat) -> CGPath? {
            guard let middle = screen(centre) else { return nil }
            let scale = projector.pointsPerMetre(east: centre.x, north: centre.y)
            let rx = max(CGFloat(radius) * scale, minimum)
            let squash = CGFloat(max(cos(projector.pitchDegrees * .pi / 180), 0.35))
            return CGPath(ellipseIn: CGRect(x: middle.x - rx, y: middle.y - rx * squash, width: rx * 2,
                                            height: rx * 2 * squash), transform: nil)
        }
        let event = playback.shot.event
        context.saveGState()
        context.setLineCap(.round)
        context.setLineJoin(.round)

        // The straight line the boom could not take, then the routes it did.
        if !event.lineOfSight, let a = screen(playback.impact), let b = screen(playback.listener) {
            context.setLineDash(phase: 0, lengths: [4, 5])
            context.setStrokeColor(ShotPalette.edge.cg(0.75))
            context.setLineWidth(1.2)
            context.strokeLineSegments(between: [a, b])
            context.setLineDash(phase: 0, lengths: [])
        }
        for route in playback.routes {
            guard let path = polyline(route.points) else { continue }
            context.setLineDash(phase: 0, lengths: route.prediction ? [6, 4] : [])
            context.setStrokeColor(ShotPalette.boom.cg(0.6))
            context.setLineWidth(2)
            context.addPath(path)
            context.strokePath()
        }
        context.setLineDash(phase: 0, lengths: [])
        for echo in playback.echoes {
            if let path = polyline(echo.leg.points) {
                context.setStrokeColor(ShotPalette.echo.cg(0.32))
                context.setLineWidth(1.2)
                context.addPath(path)
                context.strokePath()
            }
            let since = t - echo.tapTime
            if since > 0, since < 0.3, let flash = groundCircle(echo.tap, radius: 3 + 10 * since / 0.3, minimum: 5) {
                context.setFillColor(ShotPalette.echo.cg(CGFloat(0.55 * (1 - since / 0.3))))
                context.addPath(flash)
                context.fillPath()
            }
            if let mark = groundCircle(echo.tap, radius: 3.2, minimum: 4.5) {
                context.setStrokeColor(ShotPalette.echo.cg(0.95))
                context.setLineWidth(1.6)
                context.addPath(mark)
                context.strokePath()
            }
        }

        // The shell streaking in on its real track.
        if let shell = playback.shell, t < shell.impactTime + 0.4 {
            let fade = t < shell.impactTime ? 0.85 : max(0, 1 - (t - shell.impactTime) / 0.4)
            context.setAlpha(CGFloat(fade))
            let track = (0...30).compactMap { step in
                lifted(shell.position(shell.impactTime - Double(30 - step) / 30 * min(shell.span, 1200 / shell.speed)))
            }
            if track.count >= 2 {
                context.setLineDash(phase: 0, lengths: [2, 6])
                context.setStrokeColor(ShotPalette.crack.cg(1))
                context.setLineWidth(1.2)
                context.addLines(between: track)
                context.strokePath()
                context.setLineDash(phase: 0, lengths: [])
            }
            if t < shell.impactTime, let head = lifted(shell.position(t)),
               let tail = lifted(shell.position(max(shell.impactTime - shell.span, t - 0.08))) {
                context.setStrokeColor(ShotPalette.crack.cg(1))
                context.setLineWidth(3)
                context.strokeLineSegments(between: [tail, head])
                SoundPainter.disc(context, head, 3.2, fill: ShotPalette.ink.cg(1))
            }
            context.setAlpha(1)
        }

        // Impact: a flash, a mark and its label.
        let since = t - playback.boomStart
        if since > 0, since < 0.35, let flash = groundCircle(playback.impact, radius: 5 + 28 * since / 0.35, minimum: 6) {
            context.setFillColor(ShotPalette.boom.cg(CGFloat(1 - since / 0.35)))
            context.addPath(flash)
            context.fillPath()
        }
        if let mark = groundCircle(playback.impact, radius: 2.6, minimum: 4) {
            context.setFillColor(ShotPalette.boom.cg(1))
            context.addPath(mark)
            context.fillPath()
        }
        if let at = screen(playback.impact) {
            tag(context, "Impact", at: CGPoint(x: at.x + 10, y: at.y - 2), color: ShotPalette.boom)
        }

        // You: a ring for each arrival, and where it comes from.
        if let me = screen(playback.listener) {
            let hit = playback.moments.last { t >= $0.time && t < $0.time + 0.45 }
            if let hit {
                let f = (t - hit.time) / 0.45
                if let ring = groundCircle(playback.listener, radius: 4 + 18 * f, minimum: 8 + 30 * CGFloat(f)) {
                    context.setStrokeColor(hit.kind.color.cg(CGFloat(1 - f)))
                    context.setLineWidth(3)
                    context.addPath(ring)
                    context.strokePath()
                }
                if simd_length(hit.from) > 0.5,
                   let far = screen(playback.listener + hit.from * 12) {
                    let dx = far.x - me.x, dy = far.y - me.y, length = max(hypot(dx, dy), 0.001)
                    let unit = CGPoint(x: dx / length, y: dy / length)
                    let start = CGPoint(x: me.x + unit.x * 64, y: me.y + unit.y * 64)
                    let end = CGPoint(x: me.x + unit.x * 26, y: me.y + unit.y * 26)
                    context.setStrokeColor(hit.kind.color.cg(CGFloat(1 - f * 0.7)))
                    context.setLineWidth(2.5)
                    context.strokeLineSegments(between: [start, end])
                    let side = CGPoint(x: -unit.y * 6, y: unit.x * 6)
                    context.setFillColor(hit.kind.color.cg(CGFloat(1 - f * 0.7)))
                    context.addLines(between: [end, CGPoint(x: end.x + unit.x * 10 + side.x, y: end.y + unit.y * 10 + side.y),
                                               CGPoint(x: end.x + unit.x * 10 - side.x, y: end.y + unit.y * 10 - side.y)])
                    context.closePath()
                    context.fillPath()
                }
                tag(context, "You · \(hit.label)", at: CGPoint(x: me.x - 22, y: me.y - 38), color: hit.kind.color,
                    leading: false)
            }
        }
        context.restoreGState()
    }

    /// The mockup's label: mono text on a dark card.
    static func tag(_ context: CGContext, _ text: String, at anchor: CGPoint, color: RGB, leading: Bool = true) {
        let font = CTFontCreateWithName("Menlo-Bold" as CFString, 11.5, nil)
        let attributes: [NSAttributedString.Key: Any] = [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): color.cg(1),
        ]
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attributes))
        let (size, _) = TextLine.size(line)
        let origin = leading ? anchor : CGPoint(x: anchor.x - size.width, y: anchor.y)
        let card = CGRect(x: origin.x - 5, y: origin.y - 2, width: size.width + 10, height: size.height + 6)
        context.setFillColor(ShotPalette.card.cg(0.9))
        context.addPath(CGPath(roundedRect: card, cornerWidth: 5, cornerHeight: 5, transform: nil))
        context.fillPath()
        TextLine.draw(context, line, at: CGPoint(x: origin.x, y: origin.y + 1))
    }
}
