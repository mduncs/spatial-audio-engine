import SwiftUI

// Audio controls around the map. Plain shapes only (no platform controls), so
// the same views render offscreen and port to iOS unchanged.

enum Palette {
    static let panel = Color(red: 0.07, green: 0.085, blue: 0.1)
    static let card = Color(red: 0.115, green: 0.135, blue: 0.155)
    static let raised = Color(red: 0.17, green: 0.195, blue: 0.22)
    static let line = Color.white.opacity(0.08)
    static let muted = Color(red: 0.6, green: 0.65, blue: 0.69)
    static let accent = Color(red: 0.28, green: 0.86, blue: 0.74)
    static let amber = Color(red: 1, green: 0.72, blue: 0.25)
    static let glass = Color(red: 0.06, green: 0.075, blue: 0.09).opacity(0.86)
}

extension RGB {
    var color: Color { Color(.sRGB, red: red, green: green, blue: blue, opacity: 1) }
}

public struct CityMapScreen<Map: View>: View {
    @ObservedObject var store: MapStore
    let map: Map
    let scrolls: Bool

    /// `scrolls: false` lays the side panel out flat for offscreen renders.
    public init(store: MapStore, scrolls: Bool = true, @ViewBuilder map: () -> Map) {
        self.store = store
        self.scrolls = scrolls
        self.map = map()
    }

    public var body: some View {
        HStack(spacing: 0) {
            ZStack {
                map
                VStack(alignment: .leading) {
                    PlaceCard(store: store)
                    Spacer()
                    if store.shot != nil {
                        ShotStrip(store: store).frame(maxWidth: 760)
                    } else if !store.playingMusic.isEmpty {
                        MusicStrip(store: store)
                    }
                    DotLegend(store: store)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(14)
                if let notice = store.notice {
                    VStack {
                        Text(notice)
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(.black)
                            .padding(.horizontal, 14)
                            .padding(.vertical, 8)
                            .background(Capsule().fill(Palette.amber))
                            .padding(.top, 16)
                        Spacer()
                    }
                }
            }
            .clipped()
            SidePanel(store: store, scrolls: scrolls)
                .frame(width: 324)
        }
        .background(Palette.panel)
        .environment(\.colorScheme, .dark)
    }
}

struct PlaceCard: View {
    @ObservedObject var store: MapStore

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            if let state = store.state {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Circle().fill(SoundPainter.you[store.take]?.color ?? Palette.accent).frame(width: 10, height: 10)
                    Text(state.place.here).font(.system(size: 17, weight: .semibold))
                    Text("facing \(state.facing)").font(.system(size: 12)).foregroundStyle(Palette.muted)
                }
                Text(state.place.near).font(.system(size: 11.5)).foregroundStyle(Palette.muted)
            } else {
                Text("Waiting for the Workbench").font(.system(size: 15, weight: .semibold))
                Text("Start it with the City Map launcher; this map connects on its own.")
                    .font(.system(size: 11.5)).foregroundStyle(Palette.muted)
            }
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(RoundedRectangle(cornerRadius: 12).fill(Palette.glass))
    }
}

struct DotLegend: View {
    @ObservedObject var store: MapStore

    static func sample(_ context: CGContext, take: Take) {
        var dots: [(CGPoint, Double)] = []
        for column in 0..<5 {
            for row in 0..<3 {
                let quality = max(0.08, 1 - Double(column) * 0.22)
                dots.append((CGPoint(x: 6 + CGFloat(column) * 9, y: 6 + CGFloat(row) * 9), quality))
            }
        }
        DotPainter.draw(context, dots: dots, spacing: 14, pixel: 1, take: take)
    }

    var body: some View {
        HStack(spacing: 10) {
            Canvas { context, _ in
                context.withCGContext { cg in Self.sample(cg, take: store.take) }
            }
            .frame(width: 48, height: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text("Full dots: baked sound paths").font(.system(size: 11.5, weight: .medium))
                Text("Thinning dots: the estimate fades off the bake").font(.system(size: 11))
                    .foregroundStyle(Palette.muted)
            }
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 12).fill(Palette.glass))
    }
}

struct SidePanel: View {
    @ObservedObject var store: MapStore
    let scrolls: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("City Map").font(.system(size: 18, weight: .bold))
                Spacer()
                LinkBadge(status: store.status)
            }
            .padding(.bottom, 12)
            Transport(store: store)
            Divider().overlay(Palette.line).padding(.vertical, 12)
            if scrolls {
                ScrollView(.vertical, showsIndicators: false) { soundsAndSpots }
            } else {
                soundsAndSpots
            }
            Spacer(minLength: 10)
            Looks(store: store)
        }
        .foregroundStyle(.white)
        .padding(16)
        .frame(maxHeight: .infinity, alignment: .top)
        .background(Palette.panel)
        .overlay(Rectangle().fill(Palette.line).frame(width: 1), alignment: .leading)
    }

    private var soundsAndSpots: some View {
        VStack(alignment: .leading, spacing: 0) {
            Heading("Sounds")
            VStack(spacing: 2) {
                ForEach(store.sounds) { sound in
                    SoundRow(sound: sound, store: store)
                }
            }
            Divider().overlay(Palette.line).padding(.vertical, 12)
            Spots(store: store)
        }
    }
}

struct Heading: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text.uppercased())
            .font(.system(size: 10.5, weight: .semibold))
            .tracking(0.8)
            .foregroundStyle(Palette.muted)
            .padding(.bottom, 8)
    }
}

struct LinkBadge: View {
    let status: LinkClient.Status

    var body: some View {
        let linked = status == .connected
        HStack(spacing: 6) {
            Circle().fill(linked ? Palette.accent : Palette.amber).frame(width: 8, height: 8)
            Text(linked ? "Linked to Workbench" : "Waiting for Workbench")
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(linked ? .white : Palette.amber)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 5)
        .background(Capsule().fill(Palette.card))
    }
}

struct PillButton: View {
    let title: String
    let symbol: String
    var prominent = false
    var enabled = true
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 7) {
                Image(systemName: symbol).font(.system(size: 12, weight: .bold))
                Text(title).font(.system(size: 14, weight: .semibold))
            }
            .foregroundStyle(prominent ? .black : .white)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 10)
            .background(Capsule().fill(prominent ? Palette.accent : Palette.raised))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
        .opacity(enabled ? 1 : 0.4)
    }
}

struct Transport: View {
    @ObservedObject var store: MapStore

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                PillButton(title: "Play all", symbol: "play.fill", prominent: true, enabled: store.canPlay) {
                    store.playAll()
                }
                PillButton(title: "Stop all", symbol: "stop.fill", enabled: store.editable) { store.stopAll() }
            }
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Image(systemName: "speaker.wave.2.fill").font(.system(size: 12)).foregroundStyle(Palette.muted)
                    Text("Volume").font(.system(size: 13, weight: .medium))
                    Spacer()
                    Text(String(format: "%+.0f dB", store.volume))
                        .font(.system(size: 13, weight: .semibold).monospacedDigit())
                }
                VolumeSlider(value: store.volume, range: store.volumeRange, enabled: store.editable) { db, final in
                    store.setVolume(db, final: final)
                }
                Text("Workbench monitor gain. The limiter stays on.")
                    .font(.system(size: 10.5))
                    .foregroundStyle(Palette.muted)
            }
        }
    }
}

struct VolumeSlider: View {
    let value: Double
    let range: ClosedRange<Double>
    let enabled: Bool
    let onChange: (Double, Bool) -> Void

    var body: some View {
        GeometryReader { geometry in
            let width = geometry.size.width
            let fraction = (value - range.lowerBound) / (range.upperBound - range.lowerBound)
            let knob: CGFloat = 18
            let at = { (x: CGFloat) in
                range.lowerBound + Double(min(max((x - knob / 2) / (width - knob), 0), 1))
                    * (range.upperBound - range.lowerBound)
            }
            ZStack(alignment: .leading) {
                Capsule().fill(Palette.raised).frame(height: 6)
                Capsule().fill(Palette.accent).frame(width: knob / 2 + CGFloat(fraction) * (width - knob), height: 6)
                Circle().fill(.white).frame(width: knob, height: knob)
                    .shadow(color: .black.opacity(0.4), radius: 2, y: 1)
                    .offset(x: CGFloat(fraction) * (width - knob))
            }
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0)
                .onChanged { drag in onChange(at(drag.location.x), false) }
                .onEnded { drag in onChange(at(drag.location.x), true) })
        }
        .frame(height: 22)
        .disabled(!enabled)
        .opacity(enabled ? 1 : 0.4)
    }
}

struct SoundRow: View {
    let sound: Sound
    @ObservedObject var store: MapStore

    var body: some View {
        HStack(spacing: 10) {
            ZStack {
                Circle().fill(sound.on ? sound.color.color : Palette.card)
                Circle().stroke(sound.color.color, lineWidth: 2)
            }
            .frame(width: 13, height: 13)
            VStack(alignment: .leading, spacing: 2) {
                Text(sound.label).font(.system(size: 13, weight: .semibold))
                Text(detail).font(.system(size: 11)).foregroundStyle(sound.covered ? Palette.muted : Palette.amber)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Button { store.toggle(sound) } label: {
                Text(sound.on ? "On" : "Off")
                    .font(.system(size: 12, weight: .bold))
                    .foregroundStyle(sound.on ? .black : Palette.muted)
                    .frame(width: 46, height: 26)
                    .background(Capsule().fill(sound.on ? sound.color.color : Palette.raised))
                    .contentShape(Capsule())
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 5)
        .background(RoundedRectangle(cornerRadius: 10).fill(sound.selected ? Palette.card : .clear))
        .overlay(RoundedRectangle(cornerRadius: 10)
            .stroke(sound.selected ? sound.color.color.opacity(0.6) : .clear, lineWidth: 1))
        .contentShape(Rectangle())
        .onTapGesture { store.select(sound.id) }
    }

    private var detail: String {
        if !sound.covered { return "not on the baked path" }
        let place = "\(distanceText(sound.distanceM)) \(sound.direction)"
        if sound.moving { return "flies its own path · \(place)" }
        return "\(sound.size) · \(place) · ~\(Int(sound.levelDb.rounded())) dB here"
    }
}

struct Spots: View {
    @ObservedObject var store: MapStore

    var body: some View {
        let sound = store.selectedSound
        VStack(alignment: .leading, spacing: 0) {
            Heading(sound.map { "Put \($0.label) at" } ?? "Suggested spots")
            if let sound, sound.moving {
                Text("\(sound.label) flies its own path; pick another sound to place.")
                    .font(.system(size: 11.5)).foregroundStyle(Palette.muted)
            } else {
                VStack(spacing: 3) {
                    ForEach(store.hello?.spots ?? []) { spot in
                        Button { store.pick(spot) } label: {
                            HStack(spacing: 10) {
                                Image(systemName: symbol(spot.key))
                                    .font(.system(size: 13))
                                    .foregroundStyle(sound?.color.color ?? Palette.accent)
                                    .frame(width: 22)
                                VStack(alignment: .leading, spacing: 1) {
                                    Text(spot.label).font(.system(size: 12.5, weight: .semibold))
                                    Text(spot.detail).font(.system(size: 10.5)).foregroundStyle(Palette.muted)
                                        .lineLimit(1)
                                }
                                Spacer(minLength: 0)
                            }
                            .padding(.horizontal, 8)
                            .padding(.vertical, 4)
                            .background(RoundedRectangle(cornerRadius: 9).fill(Palette.card))
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .disabled(sound == nil || !store.editable)
                    }
                }
            }
        }
    }

    private func symbol(_ key: String) -> String {
        if key.hasPrefix("tower") { return "bell.fill" }
        switch key {
        case "tallest-roof": return "building.2.fill"
        case "map-corner": return "scope"
        case "street-corner": return "signpost.right.fill"
        case "overhead": return "arrow.up.to.line"
        default: return "mappin"
        }
    }
}

struct Looks: View {
    @ObservedObject var store: MapStore

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Heading("Look")
            HStack(spacing: 4) {
                Button { store.autoTake = true } label: {
                    Text("Auto")
                        .font(.system(size: 12.5, weight: .semibold))
                        .foregroundStyle(store.autoTake ? .black : .white)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 7)
                        .background(Capsule().fill(store.autoTake ? Color.white : Palette.raised))
                        .contentShape(Capsule())
                }
                .buttonStyle(.plain)
                ForEach(Take.allCases) { take in
                    let chosen = !store.autoTake && store.take == take
                    Button {
                        store.autoTake = false
                        store.take = take
                    } label: {
                        Text(take.title)
                            .font(.system(size: 12.5, weight: .semibold))
                            .foregroundStyle(chosen ? .black : .white)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 7)
                            .background(Capsule().fill(chosen ? Color.white : Palette.raised))
                            .contentShape(Capsule())
                    }
                    .buttonStyle(.plain)
                }
            }
            Text(store.autoTake ? "Towers over the city, Pins up close (now \(store.take.title))" : store.take.blurb)
                .font(.system(size: 10.5)).foregroundStyle(Palette.muted)
            Button { store.follow.toggle() } label: {
                HStack {
                    Text("Keep me in view").font(.system(size: 12.5, weight: .medium))
                    Spacer()
                    ZStack(alignment: store.follow ? .trailing : .leading) {
                        Capsule().fill(store.follow ? Palette.accent : Palette.raised).frame(width: 36, height: 20)
                        Circle().fill(.white).frame(width: 16, height: 16).padding(2)
                    }
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .padding(.top, 4)
        }
    }
}
