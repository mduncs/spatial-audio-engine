import Combine
import CoreGraphics
import Foundation

public struct RGB: Equatable, Sendable {
    public let red: CGFloat, green: CGFloat, blue: CGFloat

    public init(_ red: CGFloat, _ green: CGFloat, _ blue: CGFloat) {
        self.red = red
        self.green = green
        self.blue = blue
    }

    public init(bytes: [Int]) {
        let channel = { (index: Int) in CGFloat(bytes.indices.contains(index) ? bytes[index] : 200) / 255 }
        self.init(channel(0), channel(1), channel(2))
    }

    public func cg(_ alpha: CGFloat = 1) -> CGColor {
        CGColor(srgbRed: red, green: green, blue: blue, alpha: alpha)
    }

    public func mixed(with other: RGB, _ amount: CGFloat) -> RGB {
        RGB(red + (other.red - red) * amount, green + (other.green - green) * amount,
            blue + (other.blue - blue) * amount)
    }

    public static let white = RGB(1, 1, 1)
}

/// One sound as the map draws it: the hello's identity plus live state.
public struct Sound: Identifiable, Equatable, Sendable {
    public let id: String
    public let label: String
    public let color: RGB
    public let moving: Bool
    public var east: Double, north: Double, up: Double
    /// Where the Workbench has it; differs from `east`/`north` mid-drag
    /// when the sound is held at its last baked spot.
    public let liveEast: Double, liveNorth: Double
    public let on: Bool
    public let covered: Bool
    public let size: String
    public let widthM: Double
    public let reachM: Double
    public let distanceM: Double
    public let direction: String
    public let levelDb: Double
    public let selected: Bool
    /// Level trim a loud spot added (dB); 0 when none.
    public var boostDb: Double = 0
    /// Being dragged on this map; the position is the local preview.
    public var dragging = false
}

/// Everything the painters need for one frame.
public struct SoundScene: Sendable {
    public let listener: (east: Double, north: Double)
    public let yawDegrees: Double
    public let sounds: [Sound]
    public let take: Take
    /// Suggested spots, drawn on the map as presets for the selected sound.
    public let spots: [Hello.Spot]
    /// The selected sound's colour when it can be placed; nil otherwise.
    public let placeColor: RGB?
    /// Seconds, for the playing-sound ripples.
    public let time: Double
    /// 0...1 output level, so the ripples follow the music.
    public let pulse: Double
    /// Sounds drawn as a music field instead of ripples.
    public var fielded: Set<String> = []

    public init(listener: (east: Double, north: Double), yawDegrees: Double, sounds: [Sound], take: Take,
                spots: [Hello.Spot] = [], placeColor: RGB? = nil, time: Double = 0, pulse: Double = 0) {
        self.listener = listener
        self.yawDegrees = yawDegrees
        self.sounds = sounds
        self.take = take
        self.spots = spots
        self.placeColor = placeColor
        self.time = time
        self.pulse = pulse
    }
}

/// The map's model: link messages in, user requests out. Shared by the app
/// and the offscreen renderer (which has no link).
@MainActor
public final class MapStore: ObservableObject {
    @Published public private(set) var hello: Hello?
    @Published public private(set) var state: LinkState?
    @Published public private(set) var status: LinkClient.Status = .connecting
    @Published public private(set) var notice: String?
    @Published public var take: Take = .pins
    /// Towers over the city, Pins up close, switched by zoom.
    @Published public var autoTake = true
    @Published public var follow = true
    /// Slider position while the volume is being dragged.
    @Published public private(set) var volumeDraft: Double?
    @Published public private(set) var drag: (id: String, east: Double, north: Double)?
    /// The latest shot and its replay clock; `slow` stretches the replay.
    @Published public private(set) var shot: ShotPlayback?
    @Published public var slow: Double = 3
    /// Renders pin the replay to one time (seconds after the trigger).
    @Published public var frozenShotTime: Double?
    /// Music drawn as a living field, by sound id.
    @Published public private(set) var music: [String: MusicPlayback] = [:]
    /// Renders pin the song to one playhead (seconds).
    public var frozenMusicTime: Double?
    /// Every request also goes here (tests).
    public var outbox: ((LinkCommand) -> Void)?
    private var tracks: [String: SongTrack.Body] = [:]
    private var fields: [String: Field] = [:]
    private var musicPaths: [String: MusicPaths] = [:]
    private var playheads: [String: (seconds: Double, at: Date)] = [:]

    public let link: LinkClient?
    private var lastDragSent = Date.distantPast
    private var lastVolumeSent = Date.distantPast
    private var noticeTask: Task<Void, Never>?

    public init(link: LinkClient?) {
        self.link = link
        link?.onMessage = { [weak self] message in
            MainActor.assumeIsolated { self?.receive(message) }
        }
        link?.onStatus = { [weak self] status in
            MainActor.assumeIsolated { self?.status = status }
        }
    }

    /// A store frozen on recorded link messages (offscreen renders, tests).
    public convenience init(hello: Hello, state: LinkState, take: Take) {
        self.init(link: nil)
        self.hello = hello
        receive(.state(state))
        self.take = take
        status = .connected
    }

    public func receive(_ message: Inbound) {
        switch message {
        case let .hello(hello):
            self.hello = hello
        case let .state(state):
            if self.state != state {
                self.state = state
                for sound in state.sources {
                    guard let playhead = sound.playheadS else { continue }
                    if playheads[sound.id]?.seconds != playhead { playheads[sound.id] = (playhead, Date()) }
                    if let music = music[sound.id], music.splDb != sound.splDb { music.splDb = sound.splDb }
                }
            }
        case let .shot(shot):
            play(shot)
        case let .track(track):
            tracks[track.id] = track.track
            rebuildMusic(track.id)
        case let .field(field):
            fields[field.id] = field
            rebuildMusic(field.id)
        case let .paths(paths):
            musicPaths[paths.id] = paths
            music[paths.id]?.setPaths(paths)
        case let .notice(text):
            show(text)
        case .other:
            break
        }
    }

    /// Starts drawing a shot (live, or recorded for renders).
    public func play(_ shot: Shot, at start: Date = Date()) {
        guard let hello else { return }
        self.shot = ShotPlayback(shot: shot, hello: hello, received: start)
    }

    /// Seconds after the trigger for the shot's replay at `now`.
    public func shotTime(at now: Date = Date()) -> Double? {
        guard let shot else { return nil }
        if let frozenShotTime { return frozenShotTime }
        return shot.shot.elapsedS + now.timeIntervalSince(shot.received) / max(slow, 0.1)
    }

    /// Whether the replay is still moving (the map then animates).
    public func shotRunning(at now: Date = Date()) -> Bool {
        guard let shot, let time = shotTime(at: now) else { return false }
        return time < shot.endTime + 0.6
    }

    public func clearShot() { shot = nil }

    /// Takes recorded music lines (renders, tests) the same way as live ones.
    public func load(track: SongTrack?, field: Field?, paths: MusicPaths?) {
        if let track { receive(.track(track)) }
        if let field { receive(.field(field)) }
        if let paths { receive(.paths(paths)) }
    }

    private func rebuildMusic(_ id: String) {
        guard let hello, let field = fields[id] else { return }
        let spl = state?.sources.first(where: { $0.id == id })?.splDb ?? 110
        let playback = MusicPlayback(field: field, hello: hello, splDb: spl, track: tracks[id])
        playback.setPaths(musicPaths[id])
        music[id] = playback
    }

    /// The song's playhead for `id` at `now`; it runs on from the last state.
    public func musicTime(_ id: String, at now: Date = Date()) -> Double {
        if let frozenMusicTime { return frozenMusicTime }
        guard let playhead = playheads[id] else { return now.timeIntervalSinceReferenceDate }
        let playing = sounds.first(where: { $0.id == id })?.on ?? false
        return playhead.seconds + (playing ? now.timeIntervalSince(playhead.at) : 0)
    }

    /// Music fields to draw now: playing sounds that have one.
    public var playingMusic: [MusicPlayback] {
        let on = Set(sounds.filter(\.on).map(\.id))
        return music.values.filter { on.contains($0.id) }.sorted { $0.id < $1.id }
    }

    /// A tap on the map: walk You there. The Workbench snaps it onto the
    /// nearest street or path and refuses it inside a building; the same
    /// footprint check here answers at once.
    public func moveYou(east: Double, north: Double) {
        if hello?.building(containing: (east, north)) != nil {
            show("That's inside a building · tap a street")
            return
        }
        send(.moveYou(eastM: rounded(east), northM: rounded(north)))
    }

    private func send(_ command: LinkCommand) {
        link?.send(command)
        outbox?(command)
    }

    /// One more real shot when audio can play; otherwise a replay of the last.
    public func fireAgain() {
        if let shot, canPlay, link != nil {
            link?.send(.fire(id: shot.shot.event.sourceId))
        } else if let current = shot {
            self.shot = ShotPlayback(shot: current.shot, hello: current.hello, received: Date())
        } else if canPlay, let id = hello?.sources.first(where: { $0.id == "artillery" })?.id {
            link?.send(.fire(id: id))
        } else {
            show("Audio isn't ready to play yet")
        }
    }

    public var frame: GeoFrame? { hello?.frame }
    public var selectedID: String? { state?.selected }
    public var volumeRange: ClosedRange<Double> {
        guard let volume = hello?.volume, volume.minDb < volume.maxDb else { return -20...40 }
        return volume.minDb...volume.maxDb
    }
    public var volume: Double { volumeDraft ?? state?.volumeDb ?? 0 }
    public var canPlay: Bool { state?.canPlay ?? false }
    public var editable: Bool { state?.editable ?? false }

    public var listener: (east: Double, north: Double)? {
        guard let position = state?.listener.positionM, position.count >= 2 else { return nil }
        return (position[0], position[1])
    }

    public var sounds: [Sound] {
        guard let hello, let state else { return [] }
        return hello.sources.compactMap { source in
            guard let live = state.sources.first(where: { $0.id == source.id }), live.positionM.count >= 3 else {
                return nil
            }
            var sound = Sound(
                id: source.id, label: source.label, color: RGB(bytes: source.color), moving: source.moving,
                east: live.positionM[0], north: live.positionM[1], up: live.positionM[2],
                liveEast: live.positionM[0], liveNorth: live.positionM[1],
                on: live.on, covered: live.covered, size: live.size, widthM: live.widthM, reachM: live.reachM,
                distanceM: live.distanceM, direction: live.direction, levelDb: live.levelDb,
                selected: state.selected == source.id, boostDb: live.boostDb ?? 0)
            if let drag, drag.id == source.id {
                sound.east = drag.east
                sound.north = drag.north
                sound.dragging = true
            }
            return sound
        }
    }

    public var scene: SoundScene? { scene() }

    /// One frame's scene. The live map passes its own clock and smoothed
    /// pulse so animation never goes through SwiftUI.
    public func scene(time: Double = 0, pulse: Double? = nil) -> SoundScene? {
        guard let listener, let state else { return nil }
        let sounds = sounds
        let placeable = sounds.first(where: \.selected).flatMap { $0.moving ? nil : $0.color }
        var scene = SoundScene(listener: listener, yawDegrees: state.listener.yawDeg, sounds: sounds, take: take,
                               spots: hello?.spots ?? [], placeColor: placeable, time: time,
                               pulse: pulse ?? meterPulse)
        scene.fielded = Set(music.keys)
        return scene
    }

    /// The output meter as 0...1 (about -54 to -18 dBFS RMS).
    public var meterPulse: Double {
        guard let rms = state?.meter?.rmsDbfs else { return 0 }
        return min(max((rms + 54) / 36, 0), 1)
    }

    public var anyPlaying: Bool { state?.sources.contains(where: \.on) ?? false }

    public var selectedSound: Sound? { sounds.first(where: \.selected) }

    // MARK: requests

    public func select(_ id: String) {
        link?.send(.select(id: id))
    }

    public func toggle(_ sound: Sound) {
        if !sound.on && !canPlay {
            show("Audio isn't ready to play yet")
            return
        }
        link?.send(.setOn(id: sound.id, on: !sound.on))
    }

    public func playAll() { link?.send(.playAll) }
    public func stopAll() { link?.send(.stopAll) }

    public func setVolume(_ db: Double, final: Bool) {
        let db = min(max(db, volumeRange.lowerBound), volumeRange.upperBound).rounded()
        volumeDraft = final ? nil : db
        if final || Date().timeIntervalSince(lastVolumeSent) >= 0.06 {
            lastVolumeSent = Date()
            link?.send(.setVolume(db: db))
        }
    }

    /// A drag step on the map; the Workbench moves the sound live and holds
    /// it at its last baked spot when the ground below is not baked.
    public func drag(_ sound: Sound, east: Double, north: Double, done: Bool) {
        guard !sound.moving else {
            if done { show("\(sound.label) follows its own flight path") }
            return
        }
        drag = done ? nil : (sound.id, east, north)
        if done || Date().timeIntervalSince(lastDragSent) >= 0.05 {
            lastDragSent = Date()
            link?.send(.move(id: sound.id, eastM: rounded(east), northM: rounded(north), aboveTopM: nil, done: done))
        }
    }

    /// Puts the selected sound at a suggested spot. The Workbench owns the
    /// spot list (and any loud-spot level), so only the key goes over.
    public func pick(_ spot: Hello.Spot) {
        guard let sound = selectedSound else {
            show("Pick a sound first")
            return
        }
        guard !sound.moving else {
            show("\(sound.label) follows its own flight path")
            return
        }
        link?.send(.spot(id: sound.id, key: spot.key))
        if spot.loud { show("\(sound.label): \(spot.label.lowercased()), really, really loud") }
    }

    public func show(_ text: String) {
        notice = text
        noticeTask?.cancel()
        noticeTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 4_000_000_000)
            guard !Task.isCancelled else { return }
            self?.notice = nil
        }
    }

    private func rounded(_ metres: Double) -> Double { (metres * 100).rounded() / 100 }
}
