import SwiftUI

/// Under the map while a shot is up: Fire again, the slow-motion choice,
/// the legend, and what reaches you when (the mockup's timeline).
public struct ShotStrip: View {
    @ObservedObject var store: MapStore

    public init(store: MapStore) { self.store = store }

    public var body: some View {
        if let shot = store.shot {
            if let fixed = store.frozenShotTime {
                ShotStripBody(store: store, shot: shot, time: fixed, still: true)
            } else {
                TimelineView(.animation(minimumInterval: 1.0 / 30, paused: !store.shotRunning())) { context in
                    ShotStripBody(store: store, shot: shot, time: store.shotTime(at: context.date) ?? 0, still: false)
                }
            }
        }
    }
}

struct ShotStripBody: View {
    @ObservedObject var store: MapStore
    let shot: ShotPlayback
    let time: Double
    let still: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Button { store.fireAgain() } label: {
                    Text("Fire again")
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(.white)
                        .padding(.horizontal, 15)
                        .padding(.vertical, 8)
                        .background(Capsule().fill(ShotPalette.boom.color))
                }
                .buttonStyle(.plain)
                // The badge sits by Fire, so the open map keeps its labels.
                slowBadge
                Text(store.canPlay ? "A real shot from the Workbench · rings follow its routed paths"
                                   : "Replays the last shot · rings follow its routed paths")
                    .font(.system(size: 12))
                    .foregroundStyle(Palette.muted)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 5)
                    .background(Capsule().fill(Palette.card.opacity(0.85)))
                if !still {
                    Button { store.clearShot() } label: {
                        Image(systemName: "xmark").font(.system(size: 10, weight: .bold)).foregroundStyle(Palette.muted)
                            .padding(6).background(Circle().fill(Palette.card))
                    }
                    .buttonStyle(.plain)
                    .help("Hide the shot")
                }
                Spacer(minLength: 0)
            }
            HStack(spacing: 16) {
                swatch(ShotPalette.crack, "Supersonic crack", bar: true)
                swatch(ShotPalette.boom, "Boom, along its routed path")
                swatch(ShotPalette.echo, "Echoes off facades")
                swatch(ShotPalette.you, "You", filled: true)
            }
            .fixedSize()
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .background(Capsule().fill(Palette.glass))
            .font(.system(size: 11.5))
            .foregroundStyle(Palette.muted)
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .firstTextBaseline) {
                    Text("What reaches you, and when").font(.system(size: 13, weight: .semibold))
                    Spacer()
                    Text("real seconds after the trigger · sound at 343 m/s")
                        .font(.system(size: 10.5, design: .monospaced))
                        .foregroundStyle(Palette.muted)
                }
                ShotTrack(shot: shot, time: time)
                    .frame(height: 74)
                Text(shot.readout)
                    .font(.system(size: 12))
                    .foregroundStyle(Palette.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 11)
            .background(RoundedRectangle(cornerRadius: 13).fill(Palette.glass))
            .overlay(RoundedRectangle(cornerRadius: 13).stroke(Palette.line))
        }
        .foregroundStyle(.white)
    }

    private var slowLabel: String {
        store.slow <= 1 ? "Real time" : "Slowed \(Int(store.slow.rounded()))×"
    }

    @ViewBuilder private var slowBadge: some View {
        let badge = Text(slowLabel.uppercased())
            .font(.system(size: 10.5, weight: .semibold, design: .monospaced))
            .foregroundStyle(Palette.muted)
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .background(RoundedRectangle(cornerRadius: 6).fill(Palette.card))
            .overlay(RoundedRectangle(cornerRadius: 6).stroke(Palette.line))
        if still {
            badge
        } else {
            Menu {
                Button("Real time") { store.slow = 1 }
                Button("Slowed 3×") { store.slow = 3 }
                Button("Slowed 10×") { store.slow = 10 }
            } label: { badge }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
        }
    }

    private func swatch(_ color: RGB, _ text: String, bar: Bool = false, filled: Bool = false) -> some View {
        HStack(spacing: 6) {
            if bar {
                RoundedRectangle(cornerRadius: 2).fill(color.color).frame(width: 16, height: 5)
            } else if filled {
                Circle().fill(color.color).frame(width: 11, height: 11)
            } else {
                Circle().stroke(color.color, lineWidth: 3).frame(width: 11, height: 11)
            }
            Text(text)
        }
    }
}

/// Ticks every half second, a mark per arrival, and the playhead.
struct ShotTrack: View {
    let shot: ShotPlayback
    let time: Double

    var body: some View {
        GeometryReader { geometry in
            let total = max(1, ((shot.endTime + 0.35) * 2).rounded(.up) / 2)
            let width = geometry.size.width
            let x = { (t: Double) in CGFloat(t / total) * width }
            ZStack(alignment: .topLeading) {
                Rectangle().fill(Palette.line.opacity(3)).frame(width: width, height: 2).offset(y: 46)
                ForEach(Array(stride(from: 0.0, through: total + 1e-6, by: 0.5)), id: \.self) { tick in
                    VStack(spacing: 2) {
                        Rectangle().fill(Palette.line.opacity(3)).frame(width: 1, height: 6)
                        Text(String(format: "%.1f", tick))
                            .font(.system(size: 10, design: .monospaced))
                            .foregroundStyle(Palette.muted)
                    }
                    .fixedSize()
                    .position(x: x(tick), y: 60)
                }
                ForEach(Array(lanes().enumerated()), id: \.offset) { _, mark in
                    let moment = mark.moment
                    let lit = time >= moment.time
                    Circle()
                        .fill(moment.kind.color.color.opacity(lit ? 1 : 0.45))
                        .frame(width: 11, height: 11)
                        .overlay(Circle().stroke(Palette.panel, lineWidth: 2.5))
                        .position(x: x(moment.time), y: 47)
                    Text(moment.label)
                        .font(.system(size: 10.5, weight: .semibold, design: .monospaced))
                        .foregroundStyle(moment.kind.color.color.opacity(lit ? 1 : 0.6))
                        .fixedSize()
                        .position(x: x(moment.time), y: 33 - CGFloat(mark.lane) * 12)
                }
                Rectangle()
                    .fill(Color.white.opacity(0.6))
                    .frame(width: 2, height: 26)
                    .position(x: min(width, max(0, x(time))), y: 47)
            }
        }
    }

    /// Labels that would collide climb into the next lane.
    private func lanes() -> [(moment: ShotMoment, lane: Int)] {
        var result: [(ShotMoment, Int)] = []
        var lastTime = -Double.infinity
        var lane = 0
        let total = max(1, ((shot.endTime + 0.35) * 2).rounded(.up) / 2)
        for moment in shot.moments {
            lane = (moment.time - lastTime) / total < 0.06 ? (lane + 1) % 3 : 0
            lastTime = moment.time
            result.append((moment, lane))
        }
        return result
    }
}
