import SwiftUI

/// Under the map while music plays: what the colours mean, the song's three
/// bands right now, and how it reaches you.
public struct MusicStrip: View {
    @ObservedObject var store: MapStore

    public init(store: MapStore) { self.store = store }

    public var body: some View {
        if let music = store.playingMusic.first {
            if let fixed = store.frozenMusicTime {
                MusicStripBody(music: music, time: fixed)
            } else {
                TimelineView(.animation(minimumInterval: 1.0 / 20, paused: false)) { context in
                    MusicStripBody(music: music, time: store.musicTime(music.id, at: context.date))
                }
            }
        }
    }
}

struct MusicStripBody: View {
    let music: MusicPlayback
    let time: Double

    var body: some View {
        let bands = music.levels.bands(at: time)
        let kicked = music.levels.kicksAgo(at: time, window: 0.25).first
        HStack(spacing: 12) {
            HStack(alignment: .bottom, spacing: 4) {
                bar(MusicPalette.bass, bands.x + 6)
                bar(MusicPalette.mid, bands.y + 6)
                bar(MusicPalette.high, bands.z + 10)
            }
            .frame(width: 34, height: 26, alignment: .bottom)
            Circle()
                .fill(MusicPalette.kick.color.opacity(kicked.map { 1 - $0 / 0.25 } ?? 0.12))
                .frame(width: 9, height: 9)
            HStack(spacing: 14) {
                swatch(MusicPalette.bass, "Bass fills the streets, round corners")
                swatch(MusicPalette.mid, "Mids")
                swatch(MusicPalette.high, "Highs in line of sight")
                swatch(MusicPalette.kick, "A ripple on every kick", ring: true)
                swatch(MusicPalette.wall, "Walls that echo glow softly", bar: true)
            }
            .font(.system(size: 11.5))
            .foregroundStyle(Palette.muted)
            .fixedSize()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Capsule().fill(Palette.glass))
    }

    private func bar(_ color: RGB, _ db: Double) -> some View {
        RoundedRectangle(cornerRadius: 2)
            .fill(color.color)
            .frame(width: 8, height: max(3, min(26, 26 * (db + 24) / 28)))
    }

    private func swatch(_ color: RGB, _ text: String, ring: Bool = false, bar: Bool = false) -> some View {
        HStack(spacing: 6) {
            if bar {
                RoundedRectangle(cornerRadius: 2).fill(color.color).frame(width: 5, height: 13)
            } else if ring {
                Circle().stroke(color.color, lineWidth: 2.5).frame(width: 11, height: 11)
            } else {
                Circle().fill(RadialGradient(colors: [color.color, color.color.opacity(0)], center: .center,
                                             startRadius: 0, endRadius: 7))
                    .frame(width: 14, height: 14)
            }
            Text(text)
        }
    }
}
