import CryptoKit
import Foundation
import XCTest
@testable import FightboxKit

@available(macOS 10.15, *)
final class CanonicalAssetStreamingTests: XCTestCase {
    func testRawSyntheticChunkOpensWithBoundedResidency() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(
            "fightbox-ios-canonical-\(UUID().uuidString)",
            isDirectory: true
        )
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("chunks"),
            withIntermediateDirectories: true
        )

        let samples: [Float] = [0.25, -0.5, 0.75, -0.125]
        let raw = samples.withUnsafeBytes { Data($0) }
        let hash = SHA256.hash(data: raw).map { String(format: "%02x", $0) }.joined()
        try raw.write(to: root.appendingPathComponent("chunks/00000000.f32le"))

        let manifest = """
        {
          "schema_version":"fightbox.canonical-audio-package.v1",
          "source_descriptor_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "source_asset":{
            "schema_version":"fightbox.source-asset.v1",
            "asset_id":"ios-raw-smoke",
            "layout":"mono",
            "presentation_provenance":"native_mono",
            "compatible_geometries":["point"],
            "canonical":{
              "artifact_id":"fightbox.canonical-audio.v1:sha256:\(hash)",
              "canonical_pcm_sha256":"\(hash)",
              "frame_count":4,
              "chunk_frames":48000
            }
          },
          "sample_rate_hz":48000,
          "channel_count":1,
          "frame_count":4,
          "chunk_frames":48000,
          "compressor_revision":"zstd-rust@0.13.3|level=3",
          "chunks":[{
            "index":0,
            "start_frame":0,
            "frame_count":4,
            "file":"chunks/00000000.f32le",
            "encoding":"raw_f32_le",
            "uncompressed_bytes":16,
            "stored_bytes":16,
            "raw_sha256":"\(hash)",
            "stored_sha256":"\(hash)"
          }]
        }
        """
        try Data(manifest.utf8).write(to: root.appendingPathComponent("manifest.json"))

        let playback = try await FightboxCanonicalAssetPlayback.open(
            packageURL: root,
            sourceIndex: 0,
            expectedProgram: FightboxSpatialSourceProgram(
                channelCount: 1,
                geometry: .point
            ),
            cacheSeconds: 2
        )
        let status = playback.status()
        XCTAssertEqual(status.assetID, "ios-raw-smoke")
        XCTAssertEqual(status.residentChunkCount, 1)
        XCTAssertEqual(status.currentOriginalTimelineFrame, 0)
        XCTAssertEqual(status.memory.cacheSeconds, 2)
        XCTAssertEqual(status.memory.decodedResidentBytes, 384_000)
        XCTAssertEqual(status.underrunCount, 0)

        let provider = FightboxCanonicalProgramProvider(playbacks: [playback])
        let bank = FightboxSpatialProgramBank(
            programs: [FightboxSpatialSourceProgram(channelCount: 1, geometry: .point)],
            blockSizeFrames: 128
        )
        XCTAssertEqual(provider.fill(bank), noErr)
        bank.withChannel(sourceIndex: 0, channel: 0) { channel in
            XCTAssertEqual(Array(channel.prefix(4)), samples)
            XCTAssertTrue(channel.dropFirst(4).allSatisfy { $0 == 0 })
        }
        XCTAssertEqual(playback.status().currentOriginalTimelineFrame, 0)
        provider.commitFilledBlock()
        XCTAssertEqual(playback.status().currentOriginalTimelineFrame, 4)

        let beforeSeek = provider.programDiscontinuitySequence
        try await playback.seekForMacroArrival(
            FightboxMacroAssetArrival(programSeekFrame: 2)
        )
        playback.setActive(true)
        XCTAssertNotEqual(provider.programDiscontinuitySequence, beforeSeek)
        XCTAssertEqual(provider.fill(bank), noErr)
        bank.withChannel(sourceIndex: 0, channel: 0) { channel in
            XCTAssertEqual(Array(channel.prefix(2)), Array(samples.suffix(2)))
        }
        provider.discardFilledBlock()
        XCTAssertEqual(playback.status().currentOriginalTimelineFrame, 2)

        bank.clear()
        XCTAssertEqual(
            playback.stageMacroInterval(
                FightboxMacroProgramInterval(
                    assetFrameStart: 2,
                    frameCount: 1,
                    destinationFrameOffset: 7
                ),
                into: bank
            ),
            noErr
        )
        bank.withChannel(sourceIndex: 0, channel: 0) { channel in
            XCTAssertTrue(channel.prefix(7).allSatisfy { $0 == 0 })
            XCTAssertEqual(channel[7], samples[2])
            XCTAssertTrue(channel.dropFirst(8).allSatisfy { $0 == 0 })
        }
        XCTAssertEqual(playback.status().currentOriginalTimelineFrame, 2)
        provider.commitFilledBlock()
        XCTAssertEqual(playback.status().currentOriginalTimelineFrame, 3)

        bank.clear()
        XCTAssertEqual(
            playback.stageMacroInterval(
                FightboxMacroProgramInterval(
                    assetFrameStart: 3,
                    frameCount: 1,
                    destinationFrameOffset: 0
                ),
                into: bank
            ),
            noErr
        )
        bank.withChannel(sourceIndex: 0, channel: 0) { channel in
            XCTAssertEqual(channel[0], samples[3])
            XCTAssertTrue(channel.dropFirst().allSatisfy { $0 == 0 })
        }
        provider.commitFilledBlock()
        XCTAssertEqual(playback.status().currentOriginalTimelineFrame, 4)

        let macroPlayback = try await FightboxCanonicalAssetPlayback.open(
            packageURL: root,
            sourceIndex: 12,
            expectedProgram: FightboxSpatialSourceProgram(
                channelCount: 1,
                geometry: .point
            ),
            cacheSeconds: 2,
            active: false
        )
        let macroProvider = FightboxCanonicalProgramProvider(
            playbacks: [macroPlayback],
            macroAssetBindings: [
                FightboxMacroAssetBinding(assetKey: 77, playback: macroPlayback),
            ]
        )
        let aggregateBeforeMacroSeek = macroProvider.programDiscontinuitySequence
        let readiness = try await macroProvider.prepareMacroAsset(
            assetKey: 77,
            programSeekFrame: 1
        )
        XCTAssertEqual(
            macroProvider.programDiscontinuitySequence,
            aggregateBeforeMacroSeek,
            "pre-due macro readiness must not stall the ordinary engine callback clock"
        )
        XCTAssertEqual(readiness.sourceIndex, 12)
        XCTAssertEqual(readiness.programSeekFrame, 1)
        XCTAssertEqual(readiness.discontinuitySequence & 1, 0)

        let macroPrograms = (0 ..< 16).map { _ in
            FightboxSpatialSourceProgram(channelCount: 1, geometry: .point)
        }
        let macroBank = FightboxSpatialProgramBank(
            programs: macroPrograms,
            blockSizeFrames: 128
        )
        let requests = [
            FightboxMacroProgramRequest(
                readiness: readiness,
                interval: FightboxMacroProgramInterval(
                    assetFrameStart: 1,
                    frameCount: 2,
                    destinationFrameOffset: 5
                )
            ),
        ]
        let fillStatus = requests.withUnsafeBufferPointer {
            macroProvider.fillMacroIntervals($0, into: macroBank)
        }
        XCTAssertEqual(fillStatus, noErr)
        macroBank.withChannel(sourceIndex: 12, channel: 0) { channel in
            XCTAssertTrue(channel.prefix(5).allSatisfy { $0 == 0 })
            XCTAssertEqual(Array(channel[5 ..< 7]), Array(samples[1 ..< 3]))
            XCTAssertTrue(channel.dropFirst(7).allSatisfy { $0 == 0 })
        }
        XCTAssertEqual(macroPlayback.status().currentOriginalTimelineFrame, 1)
        macroProvider.commitFilledBlock()
        XCTAssertEqual(macroPlayback.status().currentOriginalTimelineFrame, 3)

        let successor = try await macroProvider.prepareMacroAsset(
            assetKey: 77,
            programSeekFrame: 0
        )
        XCTAssertFalse(macroProvider.releaseMacroAsset(readiness))
        XCTAssertTrue(macroProvider.releaseMacroAsset(successor))
        XCTAssertFalse(macroPlayback.status().active)
    }

    func testStableSpatialKeyRequiresOneNonzero128BitIdentity() throws {
        let bytes = Array(1 ... 16).map(UInt8.init)
        let key = try FightboxStableSpatialKey(bytes: bytes)
        XCTAssertEqual(key.bytes, bytes)
        XCTAssertThrowsError(try FightboxStableSpatialKey(bytes: [1, 2, 3]))
        XCTAssertThrowsError(
            try FightboxStableSpatialKey(
                bytes: [UInt8](repeating: 0, count: FightboxStableSpatialKey.byteCount)
            )
        )
    }
}
