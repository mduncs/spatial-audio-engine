import AppKit
import CityMapKit
import MapKit
import SwiftUI

/// Apple's live map with the quality dots as a ground overlay and the sounds
/// and you drawn on a transparent layer above it.
struct MapHost: NSViewRepresentable {
    @ObservedObject var store: MapStore

    func makeNSView(context: Context) -> MapContainer {
        MapContainer(store: store)
    }

    func updateNSView(_ view: MapContainer, context: Context) {
        view.refresh()
    }
}

final class MapContainer: NSView, MKMapViewDelegate {
    let store: MapStore
    let mapView = MKMapView()
    let layer2 = SoundLayer()
    private var dots: DotsOverlay?
    private var framedScene = false
    private var animation: Timer?
    private var pendingTap: DispatchWorkItem?

    init(store: MapStore) {
        self.store = store
        super.init(frame: .zero)
        mapView.delegate = self
        mapView.appearance = NSAppearance(named: .darkAqua)
        mapView.showsZoomControls = false
        mapView.showsPitchControl = false
        mapView.showsCompass = true
        mapView.showsScale = true
        mapView.showsUserLocation = false
        layer2.container = self
        wantsLayer = true
        if store.autoTake { store.take = .towers }
        for view in [mapView, layer2] as [NSView] {
            view.translatesAutoresizingMaskIntoConstraints = false
            addSubview(view)
            NSLayoutConstraint.activate([
                view.leadingAnchor.constraint(equalTo: leadingAnchor),
                view.trailingAnchor.constraint(equalTo: trailingAnchor),
                view.topAnchor.constraint(equalTo: topAnchor),
                view.bottomAnchor.constraint(equalTo: bottomAnchor),
            ])
        }
        configureMap(for: store.take)
        // A click on the map walks You there (a double-click still zooms).
        let click = NSClickGestureRecognizer(target: self, action: #selector(mapClicked(_:)))
        click.numberOfClicksRequired = 1
        click.delaysPrimaryMouseButtonEvents = false
        mapView.addGestureRecognizer(click)
        let double = NSClickGestureRecognizer(target: self, action: #selector(mapDoubleClicked(_:)))
        double.numberOfClicksRequired = 2
        double.delaysPrimaryMouseButtonEvents = false
        mapView.addGestureRecognizer(double)
    }

    @objc private func mapClicked(_ recognizer: NSClickGestureRecognizer) {
        guard recognizer.state == .ended, let frame = store.frame else { return }
        let coordinate = mapView.convert(recognizer.location(in: mapView), toCoordinateFrom: mapView)
        let at = frame.enu(coordinate)
        pendingTap?.cancel()
        let tap = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated { self?.store.moveYou(east: at.east, north: at.north) }
        }
        pendingTap = tap
        DispatchQueue.main.asyncAfter(deadline: .now() + NSEvent.doubleClickInterval, execute: tap)
    }

    @objc private func mapDoubleClicked(_ recognizer: NSClickGestureRecognizer) {
        pendingTap?.cancel()
        pendingTap = nil
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }

    func refresh() {
        guard let hello = store.hello, let frame = hello.frame else { return }
        if dots?.take != store.take {
            configureMap(for: store.take)
            if let dots { mapView.removeOverlay(dots) }
            let overlay = DotsOverlay(dots: hello.dots, frame: frame, take: store.take)
            mapView.addOverlay(overlay, level: .aboveRoads)
            dots = overlay
        }
        if !framedScene, let listener = store.listener {
            framedScene = true
            frameScene(hello, frame, around: listener)
        } else if store.follow {
            keepListenerInView(frame)
        }
        // While a shot is up the coverage steps back so the sound reads.
        if let dots, let renderer = mapView.renderer(for: dots) {
            let alpha: CGFloat = store.shot != nil ? 0.3 : 1
            if renderer.alpha != alpha { renderer.alpha = alpha }
        }
        animateWhileMoving()
        layer2.needsDisplay = true
    }

    /// 30 frames a second only while a shot runs or a sound plays.
    private func animateWhileMoving() {
        let moving = store.shotRunning() || store.anyPlaying
        if moving, animation == nil {
            let timer = Timer(timeInterval: 1.0 / 30, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.layer2.needsDisplay = true
                    if !(self.store.shotRunning() || self.store.anyPlaying) {
                        self.animation?.invalidate()
                        self.animation = nil
                    }
                }
            }
            RunLoop.main.add(timer, forMode: .common)
            animation = timer
        }
    }

    /// Towers when pulled back over the city, Pins up close; a short
    /// cross-fade between them, with a gap so it never flickers.
    private func followZoom() {
        guard store.autoTake else { return }
        let distance = mapView.camera.centerCoordinateDistance
        let wanted: Take = store.take == .towers ? (distance < 430 ? .pins : .towers)
                                                 : (distance > 580 ? .towers : .pins)
        guard wanted != store.take else { return }
        let fade = CATransition()
        fade.type = .fade
        fade.duration = 0.45
        layer?.add(fade, forKey: "take")
        store.take = wanted
    }

    private func configureMap(for take: Take) {
        let configuration = MKStandardMapConfiguration(elevationStyle: .realistic,
                                                       emphasisStyle: take.mutedMap ? .muted : .default)
        configuration.pointOfInterestFilter = .excludingAll
        configuration.showsTraffic = false
        mapView.preferredConfiguration = configuration
        mapView.showsBuildings = true
    }

    /// SimCity framing: pulled back, pitched, on a diagonal heading.
    private func frameScene(_ hello: Hello, _ frame: GeoFrame, around listener: (east: Double, north: Double)) {
        if hello.boundsM.count == 2, hello.boundsM.allSatisfy({ $0.count == 2 }) {
            let low = frame.coordinate(east: hello.boundsM[0][0] - 400, north: hello.boundsM[0][1] - 400)
            let high = frame.coordinate(east: hello.boundsM[1][0] + 400, north: hello.boundsM[1][1] + 400)
            let region = MKCoordinateRegion(
                center: CLLocationCoordinate2D(latitude: (low.latitude + high.latitude) / 2,
                                               longitude: (low.longitude + high.longitude) / 2),
                span: MKCoordinateSpan(latitudeDelta: high.latitude - low.latitude,
                                       longitudeDelta: high.longitude - low.longitude))
            mapView.setCameraBoundary(MKMapView.CameraBoundary(coordinateRegion: region), animated: false)
        }
        mapView.setCameraZoomRange(MKMapView.CameraZoomRange(minCenterCoordinateDistance: 120,
                                                             maxCenterCoordinateDistance: 4000), animated: false)
        mapView.camera = MKMapCamera(lookingAtCenter: frame.coordinate(east: listener.east, north: listener.north),
                                     fromDistance: 900, pitch: 52, heading: 30)
    }

    private func keepListenerInView(_ frame: GeoFrame) {
        guard let listener = store.listener else { return }
        let coordinate = frame.coordinate(east: listener.east, north: listener.north)
        let point = mapView.convert(coordinate, toPointTo: mapView)
        let middle = mapView.bounds.insetBy(dx: mapView.bounds.width * 0.2, dy: mapView.bounds.height * 0.2)
        guard !middle.contains(point) else { return }
        let camera = mapView.camera.copy() as! MKMapCamera
        camera.centerCoordinate = coordinate
        mapView.setCamera(camera, animated: true)
    }

    func mapView(_ mapView: MKMapView, rendererFor overlay: MKOverlay) -> MKOverlayRenderer {
        overlay is DotsOverlay ? DotsRenderer(overlay: overlay) : MKOverlayRenderer(overlay: overlay)
    }

    func mapViewDidChangeVisibleRegion(_ mapView: MKMapView) {
        followZoom()
        layer2.needsDisplay = true
    }
}

/// Projects through the live map view into the sound layer's coordinates.
struct LiveProjector: MapProjector {
    let mapView: MKMapView
    let target: NSView
    let frame: GeoFrame

    var pitchDegrees: Double { mapView.camera.pitch }
    var headingDegrees: Double { mapView.camera.heading }
    var eye: SIMD3<Double>? {
        let camera = mapView.camera
        let center = frame.enu(camera.centerCoordinate)
        let forward = bearingVector(camera.heading)
        let back = camera.centerCoordinateDistance * sin(camera.pitch * .pi / 180)
        return SIMD3(center.east - forward.east * back, center.north - forward.north * back,
                     camera.centerCoordinateDistance * cos(camera.pitch * .pi / 180))
    }

    func point(east: Double, north: Double) -> CGPoint? {
        let camera = mapView.camera
        let center = frame.enu(camera.centerCoordinate)
        let forward = bearingVector(camera.heading)
        let back = camera.centerCoordinateDistance * sin(camera.pitch * .pi / 180)
        let along = (east - center.east) * forward.east + (north - center.north) * forward.north
        guard along > -back + 20 else { return nil }
        return mapView.convert(frame.coordinate(east: east, north: north), toPointTo: target)
    }
}

/// Sounds and you over the map. Only speaker heads take the mouse; every
/// other click and drag falls through to the map.
final class SoundLayer: NSView {
    weak var container: MapContainer?
    private var targets: [SoundPainter.Target] = []
    private var press: (target: SoundPainter.Target, start: CGPoint, offset: CGVector, moved: Bool)?
    private let clockStart = Date()
    private var pulse = 0.0
    /// Street samples and building outlines for the current camera.
    private var shotScreen: (key: [Double], shot: ObjectIdentifier, screen: ShotScreen)?
    private var musicScreens: [ObjectIdentifier: (key: [Double], screen: MusicScreen)] = [:]

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { false }

    private var projector: LiveProjector? {
        guard let container, let frame = container.store.frame else { return nil }
        return LiveProjector(mapView: container.mapView, target: self, frame: frame)
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let container, let projector, let context = NSGraphicsContext.current?.cgContext else { return }
        let store = container.store
        let now = Date()
        // Shot light goes on the ground, under the sounds and you.
        var shotTime: Double?
        if let shot = store.shot, let time = store.shotTime(at: now) {
            let camera = container.mapView.camera
            let key = [camera.centerCoordinate.latitude, camera.centerCoordinate.longitude,
                       camera.centerCoordinateDistance, camera.heading, camera.pitch,
                       Double(bounds.width), Double(bounds.height)]
            let screen: ShotScreen
            if let cached = shotScreen, cached.key == key, cached.shot == ObjectIdentifier(shot) {
                screen = cached.screen
            } else {
                screen = ShotScreen(shot, projector: projector, bounds: bounds)
                shotScreen = (key, ObjectIdentifier(shot), screen)
            }
            ShotPainter.drawGround(context, shot, screen: screen, time: time)
            shotTime = time
        }
        pulse += (store.meterPulse - pulse) * 0.3
        // Playing music lights the streets, under the sounds and you.
        for music in store.playingMusic {
            let camera = container.mapView.camera
            let key = [camera.centerCoordinate.latitude, camera.centerCoordinate.longitude,
                       camera.centerCoordinateDistance, camera.heading, camera.pitch,
                       Double(bounds.width), Double(bounds.height)]
            let screen: MusicScreen
            if let cached = musicScreens[ObjectIdentifier(music)], cached.key == key {
                screen = cached.screen
            } else {
                screen = MusicScreen(music, projector: projector, bounds: bounds)
                musicScreens = [ObjectIdentifier(music): (key, screen)]
            }
            MusicPainter.draw(context, music, screen: screen, projector: projector,
                              time: store.musicTime(music.id, at: now), meter: pulse)
        }
        if let scene = store.scene(time: now.timeIntervalSince(clockStart), pulse: pulse) {
            targets = SoundPainter.draw(context, scene: scene, projector: projector, bounds: bounds)
        }
        if let shot = store.shot, let shotTime {
            ShotPainter.drawTop(context, shot, projector: projector, time: shotTime)
        }
    }

    private func target(at point: CGPoint) -> SoundPainter.Target? {
        targets.first { hypot($0.head.x - point.x, $0.head.y - point.y) <= $0.radius }
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        let local = convert(point, from: superview)
        return target(at: local) != nil ? self : nil
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        guard let target = target(at: point) else { return }
        press = (target, point, CGVector(dx: target.head.x - point.x, dy: target.head.y - point.y), false)
    }

    override func mouseDragged(with event: NSEvent) {
        guard var press, !press.target.isSpot, let store = container?.store else { return }
        let point = convert(event.locationInWindow, from: nil)
        if !press.moved && hypot(point.x - press.start.x, point.y - press.start.y) < 3 { return }
        press.moved = true
        self.press = press
        move(press, to: point, store: store, done: false)
    }

    override func mouseUp(with event: NSEvent) {
        guard let press, let store = container?.store else { return }
        self.press = nil
        if press.target.isSpot {
            if let spot = store.hello?.spots.first(where: { $0.key == press.target.id }) { store.pick(spot) }
            return
        }
        guard let sound = store.sounds.first(where: { $0.id == press.target.id }) else { return }
        if press.moved {
            move(press, to: convert(event.locationInWindow, from: nil), store: store, done: true)
        } else {
            store.select(sound.id)
            store.toggle(sound)
        }
    }

    private func move(_ press: (target: SoundPainter.Target, start: CGPoint, offset: CGVector, moved: Bool),
                      to point: CGPoint, store: MapStore, done: Bool) {
        guard let container, let frame = store.frame,
              let sound = store.sounds.first(where: { $0.id == press.target.id }) else { return }
        // The head floats above its ground spot; drag the ground spot.
        let ground = CGPoint(x: point.x + press.offset.dx + press.target.toGround.dx,
                             y: point.y + press.offset.dy + press.target.toGround.dy)
        let coordinate = container.mapView.convert(ground, toCoordinateFrom: self)
        let at = frame.enu(coordinate)
        store.drag(sound, east: at.east, north: at.north, done: done)
        needsDisplay = true
    }
}
