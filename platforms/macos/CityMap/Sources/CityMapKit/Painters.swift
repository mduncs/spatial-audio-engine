import CoreGraphics
import CoreText
import Foundation

/// The three visual takes on dots, speakers and you, switchable live.
public enum Take: String, CaseIterable, Identifiable, Sendable {
    case pins, glow, towers

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .pins: "Pins"
        case .glow: "Glow"
        case .towers: "Towers"
        }
    }

    public var blurb: String {
        switch self {
        case .pins: "Apple-style pins and a blue you-dot"
        case .glow: "Sounds as light; the baked path glows"
        case .towers: "SimCity: muted map, speaker towers, coverage squares"
        }
    }

    /// Towers mutes Apple's map so the coverage squares read like a data view.
    public var mutedMap: Bool { self == .towers }
}

/// Ground ENU to view coordinates (y down), for the live map and snapshots.
public protocol MapProjector {
    /// Nil when the point is behind the camera or cannot be projected.
    func point(east: Double, north: Double) -> CGPoint?
    var pitchDegrees: Double { get }
    var headingDegrees: Double { get }
    /// The camera's eye in ENU metres (`z` up), when known, so heights
    /// project through the real perspective rather than straight up.
    var eye: SIMD3<Double>? { get }
}

extension MapProjector {
    public var eye: SIMD3<Double>? { nil }

    /// A spot `up` metres above the ground. With the eye known it is exact:
    /// the ray from the eye through the raised spot meets the ground at a
    /// spot drawn in the same place, so tall buildings lean out from the
    /// middle of the view as MapKit draws them.
    public func point(east: Double, north: Double, up: Double) -> CGPoint? {
        if up == 0 { return point(east: east, north: north) }
        if let eye, eye.z > up + 1 {
            let reach = eye.z / (eye.z - up)
            return point(east: eye.x + (east - eye.x) * reach, north: eye.y + (north - eye.y) * reach)
        }
        guard let base = point(east: east, north: north) else { return nil }
        return CGPoint(x: base.x, y: base.y - liftPerMetre(east: east, north: north) * CGFloat(up))
    }

    /// View points per ground metre, measured across the view at that spot.
    public func pointsPerMetre(east: Double, north: Double) -> CGFloat {
        let heading = headingDegrees * .pi / 180
        guard let a = point(east: east, north: north),
              let b = point(east: east + 2 * cos(heading), north: north - 2 * sin(heading))
        else { return 0 }
        return hypot(b.x - a.x, b.y - a.y) / 2
    }

    /// View points per vertical metre at a ground spot (an estimate).
    public func liftPerMetre(east: Double, north: Double) -> CGFloat {
        pointsPerMetre(east: east, north: north) * CGFloat(sin(pitchDegrees * .pi / 180))
    }
}

// MARK: dots

public enum DotPainter {
    private static let levels = 8

    /// Quality dots in any y-down context. `spacing` is one grid step and
    /// `pixel` one screen pixel, both in context units.
    public static func draw(_ context: CGContext, dots: [(CGPoint, Double)], spacing: CGFloat, pixel: CGFloat,
                            take: Take) {
        guard !dots.isEmpty else { return }
        var byLevel = Array(repeating: [CGPoint](), count: levels)
        for (point, quality) in dots {
            byLevel[min(levels - 1, max(0, Int(quality * Double(levels))))].append(point)
        }
        for (level, points) in byLevel.enumerated() where !points.isEmpty {
            let quality = (CGFloat(level) + 0.5) / CGFloat(levels)
            switch take {
            case .pins:
                let radius = max(spacing * 0.17, pixel * 1.1)
                fill(context, points, radius: radius, square: false,
                     color: RGB(0.36, 0.78, 1).cg(0.2 + 0.5 * quality))
            case .glow:
                let color = RGB(0.62, 0.42, 1).mixed(with: RGB(0.25, 0.95, 0.78), quality)
                fill(context, points, radius: spacing * 0.55, square: false, color: color.cg(0.03 + 0.07 * quality))
                fill(context, points, radius: max(spacing * 0.11, pixel), square: false,
                     color: color.cg(0.35 + 0.45 * quality))
            case .towers:
                let color = RGB(1, 0.7, 0.28).mixed(with: RGB(0.42, 0.92, 0.5), quality)
                fill(context, points, radius: max(spacing * (0.11 + 0.15 * quality), pixel * 0.7), square: true,
                     color: color.cg(0.4 + 0.45 * quality))
            }
        }
    }

    private static func fill(_ context: CGContext, _ points: [CGPoint], radius: CGFloat, square: Bool, color: CGColor) {
        context.beginPath()
        for point in points {
            let rect = CGRect(x: point.x - radius, y: point.y - radius, width: radius * 2, height: radius * 2)
            if square { context.addRect(rect) } else { context.addEllipse(in: rect) }
        }
        context.setFillColor(color)
        context.fillPath()
    }
}

// MARK: text

enum TextLine {
    static func line(_ string: String, size: CGFloat, bold: Bool = false, color: CGColor) -> CTLine {
        let font = CTFontCreateUIFontForLanguage(bold ? .emphasizedSystem : .system, size, nil)
            ?? CTFontCreateWithName("Helvetica" as CFString, size, nil)
        let attributes: [NSAttributedString.Key: Any] = [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): color,
        ]
        return CTLineCreateWithAttributedString(NSAttributedString(string: string, attributes: attributes))
    }

    static func size(_ line: CTLine) -> (size: CGSize, ascent: CGFloat) {
        var ascent: CGFloat = 0, descent: CGFloat = 0, leading: CGFloat = 0
        let width = CTLineGetTypographicBounds(line, &ascent, &descent, &leading)
        return (CGSize(width: ceil(width), height: ceil(ascent + descent)), ascent)
    }

    /// Draws with its top-left at `origin` in a y-down context.
    static func draw(_ context: CGContext, _ line: CTLine, at origin: CGPoint) {
        context.saveGState()
        context.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        context.textPosition = CGPoint(x: origin.x, y: origin.y + size(line).ascent)
        CTLineDraw(line, context)
        context.restoreGState()
    }
}

// MARK: sounds and you

public enum SoundPainter {
    /// A speaker head on screen, for taps and drags.
    public struct Target: Sendable {
        /// A sound id, or a spot key when `isSpot`.
        public let id: String
        public let head: CGPoint
        public let radius: CGFloat
        /// Ground point minus head point: add to a dragged head to get ground.
        public let toGround: CGVector
        /// A suggested-spot marker: a click puts the selected sound there.
        public var isSpot = false
    }

    static let dark = RGB(0.09, 0.11, 0.13)
    static let amber = RGB(1, 0.72, 0.25)
    static let you: [Take: RGB] = [
        .pins: RGB(0.04, 0.52, 1), .glow: RGB(0.28, 0.95, 0.8), .towers: RGB(0.47, 0.86, 0.74),
    ]

    /// Draws reach, footprints, you and every speaker into a y-down context.
    @discardableResult
    public static func draw(_ context: CGContext, scene: SoundScene, projector: MapProjector,
                            bounds: CGRect) -> [Target] {
        let take = scene.take
        let far = bounds.insetBy(dx: -bounds.width, dy: -bounds.height)
        func screen(_ east: Double, _ north: Double) -> CGPoint? {
            projector.point(east: east, north: north).flatMap { far.contains($0) ? $0 : nil }
        }
        var taken: [CGRect] = []

        // A playing song lights its own reach street by street; no ring.
        if let selected = scene.sounds.first(where: \.selected), selected.reachM > 0,
           !(selected.on && scene.fielded.contains(selected.id)) {
            reachRing(context, selected, screen: screen, bounds: bounds)
        }
        for sound in scene.sounds where !sound.moving && sound.widthM >= 1 {
            footprint(context, sound, screen: screen)
        }
        for sound in scene.sounds where sound.on && !scene.fielded.contains(sound.id) {
            ripples(context, sound, scene: scene, screen: screen)
        }
        let spotMarks = spotMarkers(context, scene: scene, projector: projector, screen: screen, bounds: bounds)
        let me = screen(scene.listener.east, scene.listener.north)
        let facing = me.map { facingAngle(at: $0, scene: scene, projector: projector) } ?? 0
        let size = youRadius(projector, scene.listener)
        if let me {
            cone(context, at: me, angle: facing, radius: size, take: take)
            taken.append(CGRect(x: me.x - size * 1.3, y: me.y - size * 1.3, width: size * 2.6, height: size * 2.6))
        }

        var placed: [(Sound, CGPoint, CGPoint, CGFloat)] = []
        for sound in scene.sounds {
            guard let ground = screen(sound.east, sound.north) else { continue }
            let lift = min(CGFloat(max(sound.up, 0)) * projector.liftPerMetre(east: sound.east, north: sound.north),
                           bounds.height * 0.35)
            let rise = lift + baseRise(take)
            let head = CGPoint(x: ground.x, y: max(ground.y - rise, bounds.minY + 24))
            placed.append((sound, ground, head, headRadius(take)))
        }
        // Far ones first, the selected one on top.
        placed.sort { a, b in a.0.selected == b.0.selected ? a.1.y < b.1.y : !a.0.selected }
        for (sound, ground, head, _) in placed {
            if sound.dragging, let live = screen(sound.liveEast, sound.liveNorth),
               hypot(live.x - ground.x, live.y - ground.y) > 6 {
                held(context, sound, live: live, preview: ground)
            }
            speaker(context, sound, ground: ground, head: head, take: take)
            if sound.on { headPulse(context, sound, head: head, scene: scene) }
        }
        // You stay on top of every speaker.
        if let me { puck(context, at: me, angle: facing, radius: size, take: take) }
        var targets: [Target] = []
        for (sound, ground, head, radius) in placed {
            taken.append(CGRect(x: head.x - radius - 3, y: head.y - radius - 3, width: radius * 2 + 6,
                                height: radius * 2 + 6))
            targets.append(Target(id: sound.id, head: head, radius: radius + 5,
                                  toGround: CGVector(dx: ground.x - head.x, dy: ground.y - head.y)))
        }
        for (sound, _, head, radius) in placed.reversed() {
            label(context, sound, head: head, radius: radius, take: take, taken: &taken, bounds: bounds)
        }
        for mark in spotMarks {
            taken.append(CGRect(x: mark.head.x - 11, y: mark.head.y - 11, width: 22, height: 22))
        }
        for (mark, spot) in zip(spotMarks, scene.spots.filter { !$0.followsListener }) where mark.id == spot.key {
            spotLabel(context, spot, at: mark.head, color: scene.placeColor, taken: &taken, bounds: bounds)
        }
        // Speaker heads win over spot markers under the mouse.
        return targets.reversed() + spotMarks
    }

    static func headRadius(_ take: Take) -> CGFloat {
        switch take {
        case .pins: 14
        case .glow: 9
        case .towers: 12
        }
    }

    static func baseRise(_ take: Take) -> CGFloat {
        switch take {
        case .pins: 30
        case .glow: 0
        case .towers: 22
        }
    }

    private static func ring(_ east: Double, _ north: Double, radius: Double, steps: Int,
                             screen: (Double, Double) -> CGPoint?) -> [CGPoint?] {
        (0...steps).map { step in
            let angle = Double(step) / Double(steps) * 2 * .pi
            return screen(east + radius * cos(angle), north + radius * sin(angle))
        }
    }

    private static func reachRing(_ context: CGContext, _ sound: Sound, screen: (Double, Double) -> CGPoint?,
                                  bounds: CGRect) {
        let points = ring(sound.east, sound.north, radius: sound.reachM, steps: 160, screen: screen)
        context.saveGState()
        context.beginPath()
        var drawing = false
        var top: CGPoint?
        for point in points {
            guard let point else { drawing = false; continue }
            if drawing { context.addLine(to: point) } else { context.move(to: point); drawing = true }
            if bounds.insetBy(dx: 40, dy: 30).contains(point), top.map({ point.y < $0.y }) ?? true { top = point }
        }
        context.setStrokeColor(sound.color.cg(0.8))
        context.setLineWidth(1.6)
        context.setLineDash(phase: 0, lengths: [7, 5])
        context.strokePath()
        context.restoreGState()
        if let top {
            let line = TextLine.line("\(sound.label) carries ~\(distanceText(sound.reachM))", size: 11.5, bold: true,
                                 color: sound.color.cg())
            let size = TextLine.size(line).size
            context.saveGState()
            context.setShadow(offset: .zero, blur: 4, color: CGColor(gray: 0, alpha: 0.9))
            TextLine.draw(context, line, at: CGPoint(x: top.x - size.width / 2, y: top.y + 4))
            context.restoreGState()
        }
    }

    private static func footprint(_ context: CGContext, _ sound: Sound, screen: (Double, Double) -> CGPoint?) {
        let points = ring(sound.east, sound.north, radius: sound.widthM / 2, steps: 40, screen: screen)
        guard let first = points.first ?? nil, points.allSatisfy({ $0 != nil }) else { return }
        let compact = points.compactMap { $0 }
        guard compact.map({ hypot($0.x - first.x, $0.y - first.y) }).max() ?? 0 > 5 else { return }
        context.beginPath()
        context.addLines(between: compact)
        context.closePath()
        context.setFillColor(sound.color.cg(0.28))
        context.fillPath()
    }

    // MARK: you

    /// The you-marker radius: never small, and bigger as you zoom in.
    static func youRadius(_ projector: MapProjector, _ listener: (east: Double, north: Double)) -> CGFloat {
        let perMetre = projector.pointsPerMetre(east: listener.east, north: listener.north)
        return min(max(perMetre * 3.5, 19), 32)
    }

    private static func facingAngle(at me: CGPoint, scene: SoundScene, projector: MapProjector) -> CGFloat {
        let forward = bearingVector(scene.yawDegrees)
        let ahead = projector.point(east: scene.listener.east + forward.east * 8,
                                    north: scene.listener.north + forward.north * 8)
        if let ahead, hypot(ahead.x - me.x, ahead.y - me.y) > 0.5 {
            return atan2(ahead.y - me.y, ahead.x - me.x)
        }
        return CGFloat((scene.yawDegrees - projector.headingDegrees - 90) * .pi / 180)
    }

    private static func wedge(at me: CGPoint, angle: CGFloat, radius: CGFloat,
                              take: Take) -> (path: CGPath, reach: CGFloat, half: CGFloat) {
        let (reach, half): (CGFloat, CGFloat) = switch take {
        case .pins: (max(radius * 7.5, 190), 0.44)
        case .glow: (max(radius * 9, 220), 0.3)
        case .towers: (max(radius * 7, 170), 0.46)
        }
        let wedge = CGMutablePath()
        wedge.move(to: me)
        wedge.addArc(center: me, radius: reach, startAngle: angle - half, endAngle: angle + half, clockwise: false)
        wedge.closeSubpath()
        return (wedge, reach, half)
    }

    /// Your heading beam: a fixed screen size (bigger when zoomed in) so it
    /// reads at every zoom, with crisp edges so the direction is unmistakable.
    private static func cone(_ context: CGContext, at me: CGPoint, angle: CGFloat, radius: CGFloat, take: Take) {
        let color = you[take] ?? .white
        let (wedge, reach, half) = wedge(at: me, angle: angle, radius: radius, take: take)
        let peak: CGFloat = take == .towers ? 0.5 : 0.82
        context.saveGState()
        context.addPath(wedge)
        context.clip()
        if let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
                                     colors: [color.cg(peak), color.cg(peak * 0.55), color.cg(0)] as CFArray,
                                     locations: [0, 0.55, 1]) {
            context.drawRadialGradient(gradient, startCenter: me, startRadius: radius * 0.6, endCenter: me,
                                       endRadius: reach, options: [.drawsBeforeStartLocation])
        }
        context.restoreGState()
        // Edges, fading out along the beam.
        context.saveGState()
        context.setLineCap(.round)
        for side in [angle - half, angle + half] {
            let steps = 6
            for step in 0..<steps {
                let from = radius * 0.9 + (reach * 0.9 - radius * 0.9) * CGFloat(step) / CGFloat(steps)
                let to = radius * 0.9 + (reach * 0.9 - radius * 0.9) * CGFloat(step + 1) / CGFloat(steps)
                context.setStrokeColor(color.mixed(with: .white, 0.35).cg(0.85 * (1 - CGFloat(step) / CGFloat(steps))))
                context.setLineWidth(take == .glow ? 1.4 : 2)
                if take == .towers { context.setLineDash(phase: 0, lengths: [5, 4]) }
                context.strokeLineSegments(between: [
                    CGPoint(x: me.x + cos(side) * from, y: me.y + sin(side) * from),
                    CGPoint(x: me.x + cos(side) * to, y: me.y + sin(side) * to),
                ])
            }
        }
        context.restoreGState()
    }

    /// The you marker itself, sized by `radius`, with a chevron for facing.
    private static func puck(_ context: CGContext, at me: CGPoint, angle: CGFloat, radius: CGFloat, take: Take) {
        let color = you[take] ?? .white
        let along = CGVector(dx: cos(angle), dy: sin(angle)), side = CGVector(dx: -sin(angle), dy: cos(angle))
        func at(_ forward: CGFloat, _ across: CGFloat) -> CGPoint {
            CGPoint(x: me.x + along.dx * forward + side.dx * across, y: me.y + along.dy * forward + side.dy * across)
        }
        func chevron(_ scale: CGFloat, fill: CGColor) {
            let arrow = CGMutablePath()
            arrow.addLines(between: [at(scale, 0), at(-scale * 0.7, scale * 0.78), at(-scale * 0.3, 0),
                                     at(-scale * 0.7, -scale * 0.78)])
            arrow.closeSubpath()
            context.addPath(arrow)
            context.setFillColor(fill)
            context.fillPath()
        }
        switch take {
        case .pins:
            disc(context, me, radius * 1.8, fill: color.cg(0.18))
            context.saveGState()
            context.setShadow(offset: CGSize(width: 0, height: 2), blur: 7, color: CGColor(gray: 0, alpha: 0.65))
            disc(context, me, radius, fill: CGColor(gray: 1, alpha: 1))
            context.restoreGState()
            disc(context, me, radius * 0.74, fill: color.cg())
            chevron(radius * 0.5, fill: CGColor(gray: 1, alpha: 1))
        case .glow:
            if let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
                                         colors: [color.cg(0.85), color.cg(0)] as CFArray, locations: [0, 1]) {
                context.drawRadialGradient(gradient, startCenter: me, startRadius: 0, endCenter: me,
                                           endRadius: radius * 2.6, options: [])
            }
            disc(context, me, radius * 0.62, fill: CGColor(gray: 1, alpha: 1))
            ring(context, me, radius * 0.9, color: color.cg(), width: 3)
            chevron(radius * 0.42, fill: color.mixed(with: dark, 0.3).cg())
        case .towers:
            let scale = radius / 19
            let arrow = CGMutablePath()
            arrow.addLines(between: [at(30 * scale, 0), at(-16 * scale, 20 * scale), at(-7 * scale, 0),
                                     at(-16 * scale, -20 * scale)])
            arrow.closeSubpath()
            context.addPath(arrow)
            context.setFillColor(color.cg())
            context.setStrokeColor(dark.cg())
            context.setLineWidth(3)
            context.setLineJoin(.round)
            context.drawPath(using: .fillStroke)
        }
    }

    // MARK: playing

    /// Ground ripples spreading from a playing sound, following the output
    /// level: the music you can see.
    private static func ripples(_ context: CGContext, _ sound: Sound, scene: SoundScene,
                                screen: (Double, Double) -> CGPoint?) {
        let reach = min(max(sound.reachM * 0.06, 16), 55)
        let strength = 0.4 + 0.6 * scene.pulse
        context.saveGState()
        for wave in 0..<3 {
            let phase = (scene.time / 1.6 + Double(wave) / 3).truncatingRemainder(dividingBy: 1)
            let points = ring(sound.east, sound.north, radius: 1.5 + phase * reach, steps: 48, screen: screen)
            guard points.allSatisfy({ $0 != nil }) else { continue }
            context.beginPath()
            context.addLines(between: points.compactMap { $0 })
            context.closePath()
            let fade = CGFloat(pow(1 - phase, 1.4) * strength)
            context.setStrokeColor(sound.color.mixed(with: .white, 0.25).cg(0.75 * fade))
            context.setLineWidth(1 + 2.2 * fade)
            context.strokePath()
        }
        context.restoreGState()
    }

    /// One ring breathing out of a playing speaker's head.
    private static func headPulse(_ context: CGContext, _ sound: Sound, head: CGPoint, scene: SoundScene) {
        let phase = CGFloat((scene.time / 1.1).truncatingRemainder(dividingBy: 1))
        let radius = headRadius(scene.take) + 4 + phase * (14 + 10 * CGFloat(scene.pulse))
        ring(context, head, radius, color: sound.color.cg((1 - phase) * (0.35 + 0.5 * CGFloat(scene.pulse))),
             width: 2)
    }

    // MARK: spots

    /// Suggested spots as presets on the map: a marker at the spot's height
    /// over its street, in the selected sound's colour when it can go there.
    private static func spotMarkers(_ context: CGContext, scene: SoundScene, projector: MapProjector,
                                    screen: (Double, Double) -> CGPoint?, bounds: CGRect) -> [Target] {
        let color = scene.placeColor ?? RGB(0.78, 0.82, 0.86)
        var targets: [Target] = []
        for spot in scene.spots where !spot.followsListener {
            guard let ground = screen(spot.eastM, spot.northM),
                  bounds.insetBy(dx: -10, dy: -10).contains(ground) else { continue }
            let lift = min(CGFloat(max(spot.upM, 0)) * projector.liftPerMetre(east: spot.eastM, north: spot.northM),
                           bounds.height * 0.35)
            let head = CGPoint(x: ground.x, y: max(ground.y - lift - 8, bounds.minY + 16))
            context.saveGState()
            context.setStrokeColor(color.cg(0.55))
            context.setLineWidth(1.2)
            context.setLineDash(phase: 0, lengths: [2, 3])
            context.strokeLineSegments(between: [ground, head])
            context.restoreGState()
            disc(context, ground, 3, fill: color.cg(0.7), squash: 0.5)
            let size: CGFloat = spot.loud ? 9 : 7.5
            let diamond = CGMutablePath()
            diamond.addLines(between: [CGPoint(x: head.x, y: head.y - size), CGPoint(x: head.x + size, y: head.y),
                                       CGPoint(x: head.x, y: head.y + size), CGPoint(x: head.x - size, y: head.y)])
            diamond.closeSubpath()
            context.saveGState()
            context.setShadow(offset: CGSize(width: 0, height: 1), blur: 3, color: CGColor(gray: 0, alpha: 0.6))
            context.addPath(diamond)
            context.setFillColor(dark.cg(0.92))
            context.fillPath()
            context.restoreGState()
            context.addPath(diamond)
            context.setStrokeColor(spot.loud ? amber.cg() : color.cg())
            context.setLineWidth(2)
            context.strokePath()
            disc(context, head, spot.loud ? 3.2 : 2.4, fill: spot.loud ? amber.cg() : color.cg())
            if spot.loud {
                for (offset, alpha) in [(5.0, 0.6), (10.0, 0.3)] as [(CGFloat, CGFloat)] {
                    ring(context, head, size + offset, color: amber.cg(alpha), width: 1.5)
                }
            }
            targets.append(Target(id: spot.key, head: head, radius: 13,
                                  toGround: CGVector(dx: ground.x - head.x, dy: ground.y - head.y), isSpot: true))
        }
        return targets
    }

    private static func spotLabel(_ context: CGContext, _ spot: Hello.Spot, at head: CGPoint, color: RGB?,
                                  taken: inout [CGRect], bounds: CGRect) {
        let title = TextLine.line(spot.label, size: 10.5, bold: true,
                                  color: spot.loud ? amber.cg() : CGColor(gray: 0.92, alpha: 1))
        let size = TextLine.size(title).size
        let candidates = [
            CGRect(x: head.x + 14, y: head.y - size.height / 2, width: size.width, height: size.height),
            CGRect(x: head.x - 14 - size.width, y: head.y - size.height / 2, width: size.width, height: size.height),
        ]
        guard let rect = candidates.first(where: { rect in
            bounds.insetBy(dx: 6, dy: 6).contains(rect.insetBy(dx: -5, dy: -3))
                && !taken.contains { $0.intersects(rect.insetBy(dx: -6, dy: -4)) }
        }) else { return }
        let box = rect.insetBy(dx: -5, dy: -2.5)
        context.addPath(CGPath(roundedRect: box, cornerWidth: 5, cornerHeight: 5, transform: nil))
        context.setFillColor(dark.cg(0.78))
        context.fillPath()
        TextLine.draw(context, title, at: rect.origin)
        taken.append(box)
    }

    // MARK: speakers

    private static func held(_ context: CGContext, _ sound: Sound, live: CGPoint, preview: CGPoint) {
        context.saveGState()
        context.setStrokeColor(sound.color.cg(0.7))
        context.setLineWidth(1.5)
        context.setLineDash(phase: 0, lengths: [3, 4])
        context.strokeLineSegments(between: [live, preview])
        context.restoreGState()
        ring(context, live, 7, color: sound.color.cg(0.9), width: 2)
    }

    private static func speaker(_ context: CGContext, _ sound: Sound, ground: CGPoint, head: CGPoint, take: Take) {
        let color = sound.color
        let radius = headRadius(take)
        switch take {
        case .pins:
            disc(context, ground, 4, fill: CGColor(gray: 0, alpha: 0.45), squash: 0.45)
            context.setStrokeColor(color.mixed(with: dark, 0.25).cg())
            context.setLineWidth(2.5)
            context.setLineCap(.round)
            context.strokeLineSegments(between: [ground, CGPoint(x: head.x, y: head.y + radius - 1)])
            if sound.on {
                ring(context, head, radius + 7, color: color.cg(0.45), width: 2)
                ring(context, head, radius + 13, color: color.cg(0.2), width: 1.5)
            }
            context.saveGState()
            context.setShadow(offset: CGSize(width: 0, height: sound.dragging ? 5 : 2), blur: sound.dragging ? 10 : 5,
                              color: CGColor(gray: 0, alpha: 0.55))
            disc(context, head, radius, fill: sound.on ? color.cg() : dark.cg())
            context.restoreGState()
            ring(context, head, radius - 1.25, color: sound.on ? CGColor(gray: 1, alpha: 1) : color.cg(), width: 2.5)
            if sound.selected { ring(context, head, radius + 3.5, color: CGColor(gray: 1, alpha: 1), width: 2.5) }
            glyph(context, head, size: radius * 0.62, color: sound.on ? CGColor(gray: 1, alpha: 1) : color.cg(),
                  playing: sound.on)
        case .glow:
            disc(context, ground, 9, fill: color.cg(0.3), squash: 0.4)
            if head.y < ground.y - 2 {
                context.saveGState()
                context.setStrokeColor(color.cg(0.5))
                context.setLineWidth(1.5)
                context.setLineDash(phase: 0, lengths: [2, 3])
                context.strokeLineSegments(between: [ground, head])
                context.restoreGState()
            }
            if sound.on {
                if let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
                                             colors: [color.cg(0.75), color.cg(0)] as CFArray, locations: [0, 1]) {
                    context.drawRadialGradient(gradient, startCenter: head, startRadius: 0, endCenter: head,
                                               endRadius: 40, options: [])
                }
                for (offset, alpha) in [(7.0, 0.55), (14.0, 0.35), (21.0, 0.18)] as [(CGFloat, CGFloat)] {
                    ring(context, head, radius + offset, color: color.cg(alpha), width: 1.5)
                }
                disc(context, head, radius, fill: color.mixed(with: .white, 0.4).cg())
            } else {
                disc(context, head, radius, fill: dark.cg(0.75))
                ring(context, head, radius - 1, color: color.cg(0.85), width: 2)
            }
            if sound.selected { ring(context, head, radius + 4, color: CGColor(gray: 1, alpha: 0.95), width: 2) }
        case .towers:
            let top = CGPoint(x: head.x, y: head.y + radius)
            if ground.y > top.y + 1 {
                let mast = CGMutablePath()
                let half: CGFloat = 3
                mast.move(to: CGPoint(x: ground.x - half, y: ground.y))
                mast.addLine(to: CGPoint(x: top.x - half * 0.6, y: top.y))
                mast.move(to: CGPoint(x: ground.x + half, y: ground.y))
                mast.addLine(to: CGPoint(x: top.x + half * 0.6, y: top.y))
                var y = ground.y, left = true
                while y - 6 > top.y {
                    let next = y - 6
                    let width = half * (0.6 + 0.4 * (next - top.y) / max(ground.y - top.y, 1))
                    mast.move(to: CGPoint(x: ground.x + (left ? -half : half), y: y))
                    mast.addLine(to: CGPoint(x: ground.x + (left ? width : -width), y: next))
                    y = next
                    left.toggle()
                }
                context.addPath(mast)
                context.setStrokeColor(CGColor(gray: 0.88, alpha: 0.9))
                context.setLineWidth(1.1)
                context.strokePath()
            }
            let base = CGRect(x: ground.x - 6, y: ground.y - 2.5, width: 12, height: 5)
            context.setFillColor(dark.cg())
            context.fill(base)
            context.setStrokeColor(color.cg())
            context.setLineWidth(1.2)
            context.stroke(base)
            if sound.on {
                for (step, alpha) in [(0, 0.85), (1, 0.55), (2, 0.3)] as [(Int, CGFloat)] {
                    let arc = radius + 5 + CGFloat(step) * 5.5
                    for side in [0.0, CGFloat.pi] {
                        context.beginPath()
                        context.addArc(center: head, radius: arc, startAngle: side - 0.6, endAngle: side + 0.6,
                                       clockwise: false)
                        context.setStrokeColor(color.cg(alpha))
                        context.setLineWidth(2)
                        context.setLineCap(.round)
                        context.strokePath()
                    }
                }
            }
            let box = CGRect(x: head.x - radius, y: head.y - radius, width: radius * 2, height: radius * 2)
            let shape = CGPath(roundedRect: box, cornerWidth: 4, cornerHeight: 4, transform: nil)
            context.saveGState()
            context.setShadow(offset: CGSize(width: 0, height: 2), blur: 4, color: CGColor(gray: 0, alpha: 0.5))
            context.addPath(shape)
            context.setFillColor(sound.on ? color.cg() : dark.cg())
            context.fillPath()
            context.restoreGState()
            context.addPath(shape)
            context.setStrokeColor(sound.on ? CGColor(gray: 0.95, alpha: 1) : color.cg())
            context.setLineWidth(1.5)
            context.strokePath()
            if sound.selected {
                context.addPath(CGPath(roundedRect: box.insetBy(dx: -4, dy: -4), cornerWidth: 6, cornerHeight: 6,
                                       transform: nil))
                context.setStrokeColor(CGColor(gray: 1, alpha: 1))
                context.setLineWidth(2)
                context.strokePath()
            }
            glyph(context, head, size: radius * 0.66, color: sound.on ? dark.cg() : color.cg(), playing: sound.on)
        }
        if !sound.covered {
            let badge = CGPoint(x: head.x + radius * 0.75, y: head.y - radius * 0.75)
            disc(context, badge, 5.5, fill: amber.cg())
            ring(context, badge, 5.5, color: dark.cg(), width: 1.5)
        }
    }

    private static func label(_ context: CGContext, _ sound: Sound, head: CGPoint, radius: CGFloat, take: Take,
                              taken: inout [CGRect], bounds: CGRect) {
        var detail = "\(sound.size) · \(distanceText(sound.distanceM))"
        if sound.up >= 4 { detail += " · \(Int(sound.up.rounded())) m up" }
        if sound.boostDb > 0 { detail += " · +\(Int(sound.boostDb.rounded())) dB loud" }
        if !sound.covered { detail = "not on the baked path" }
        let title = TextLine.line(sound.label, size: 12.5, bold: true,
                              color: take == .glow ? sound.color.mixed(with: .white, 0.35).cg() : CGColor(gray: 1, alpha: 1))
        let sub = TextLine.line(detail, size: 10.5, color: sound.covered ? CGColor(gray: 0.78, alpha: 1) : amber.cg())
        let titleSize = TextLine.size(title).size, subSize = TextLine.size(sub).size
        let size = CGSize(width: max(titleSize.width, subSize.width), height: titleSize.height + subSize.height + 1)
        let pad = CGSize(width: take == .towers ? 10 : 8, height: 4)
        let candidates = [
            CGRect(x: head.x + radius + 8 + pad.width, y: head.y - size.height / 2, width: size.width,
                   height: size.height),
            CGRect(x: head.x - radius - 8 - pad.width - size.width, y: head.y - size.height / 2, width: size.width,
                   height: size.height),
        ]
        let fits = { (rect: CGRect) in
            bounds.insetBy(dx: 4, dy: 4).contains(rect.insetBy(dx: -pad.width, dy: -pad.height))
        }
        let free = candidates.first { rect in
            fits(rect) && !taken.contains { $0.intersects(rect.insetBy(dx: -pad.width - 2, dy: -pad.height - 2)) }
        }
        guard let rect = free ?? (sound.selected || sound.dragging ? candidates.first(where: fits) : nil) else { return }
        let box = rect.insetBy(dx: -pad.width, dy: -pad.height)
        switch take {
        case .pins:
            context.addPath(CGPath(roundedRect: box, cornerWidth: 8, cornerHeight: 8, transform: nil))
            context.setFillColor(dark.cg(0.86))
            context.fillPath()
        case .glow:
            context.saveGState()
            context.setShadow(offset: .zero, blur: 6, color: CGColor(gray: 0, alpha: 1))
        case .towers:
            context.setFillColor(dark.cg(0.92))
            context.fill(box)
            context.setFillColor(sound.color.cg())
            context.fill(CGRect(x: box.minX, y: box.minY, width: 3, height: box.height))
        }
        TextLine.draw(context, title, at: rect.origin)
        TextLine.draw(context, sub, at: CGPoint(x: rect.minX, y: rect.minY + titleSize.height + 1))
        if take == .glow { context.restoreGState() }
        taken.append(box)
    }

    // MARK: primitives

    static func disc(_ context: CGContext, _ center: CGPoint, _ radius: CGFloat, fill: CGColor, squash: CGFloat = 1) {
        context.setFillColor(fill)
        context.fillEllipse(in: CGRect(x: center.x - radius, y: center.y - radius * squash, width: radius * 2,
                                       height: radius * 2 * squash))
    }

    static func ring(_ context: CGContext, _ center: CGPoint, _ radius: CGFloat, color: CGColor, width: CGFloat) {
        context.setStrokeColor(color)
        context.setLineWidth(width)
        context.strokeEllipse(in: CGRect(x: center.x - radius, y: center.y - radius, width: radius * 2,
                                         height: radius * 2))
    }

    /// A small loudspeaker; with sound waves while it plays.
    static func glyph(_ context: CGContext, _ center: CGPoint, size: CGFloat, color: CGColor, playing: Bool) {
        let shift = playing ? -size * 0.25 : 0
        let x = center.x + shift
        let cone = CGMutablePath()
        cone.addLines(between: [
            CGPoint(x: x - size * 0.75, y: center.y - size * 0.3), CGPoint(x: x - size * 0.4, y: center.y - size * 0.3),
            CGPoint(x: x + size * 0.15, y: center.y - size * 0.75), CGPoint(x: x + size * 0.15, y: center.y + size * 0.75),
            CGPoint(x: x - size * 0.4, y: center.y + size * 0.3), CGPoint(x: x - size * 0.75, y: center.y + size * 0.3),
        ])
        cone.closeSubpath()
        context.addPath(cone)
        context.setFillColor(color)
        context.fillPath()
        guard playing else { return }
        for radius in [size * 0.5, size * 0.9] {
            context.beginPath()
            context.addArc(center: CGPoint(x: x + size * 0.15, y: center.y), radius: radius, startAngle: -0.75,
                           endAngle: 0.75, clockwise: false)
            context.setStrokeColor(color)
            context.setLineWidth(max(size * 0.17, 1.2))
            context.setLineCap(.round)
            context.strokePath()
        }
    }
}
