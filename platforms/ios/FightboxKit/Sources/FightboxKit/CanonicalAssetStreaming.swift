#if canImport(FightboxC)
import FightboxC
#endif
import AudioToolbox
import CryptoKit
import Darwin
import Foundation

private let canonicalPackageSchema = "fightbox.canonical-audio-package.v1"
private let canonicalSourceSchema = "fightbox.source-asset.v1"
private let canonicalSampleRate: UInt32 = 48_000
private let canonicalChunkFrames = 48_000
private let canonicalCompressorRevision = "zstd-rust@0.13.3|level=3"
private let canonicalInvalidChunk = UInt64.max

private let monoExpansionNearDelay = 257
private let monoExpansionFarDelay = 1_103
private let monoExpansionSideGain = 3.0 / 16.0
private let monoExpansionRecipeSHA256 =
    "f32a78304c3f6116233b442a879883e1dec9d80f142eb0041518cfec483e0c91"

public enum FightboxCanonicalAssetLayout: String, Codable, Sendable {
    case mono
    case stereoLR = "stereo_lr"

    public var storedChannelCount: Int {
        switch self {
        case .mono: return 1
        case .stereoLR: return 2
        }
    }
}

public enum FightboxCanonicalPresentationProvenance: String, Codable, Sendable {
    case nativeMono = "native_mono"
    case authoredStereo = "authored_stereo"
    case monoExpanded = "mono_expanded"

    public var deliveredProgramPlaneCount: Int {
        switch self {
        case .nativeMono: return 1
        case .authoredStereo, .monoExpanded: return 2
        }
    }
}

public enum FightboxCanonicalSourceGeometry: String, Codable, Sendable {
    case point
    case multiPoint = "multi_point"
    case lineSegment = "line_segment"
    case stereoImage = "stereo_image"
}

public enum FightboxCanonicalChunkEncoding: String, Codable, Sendable {
    case rawF32LE = "raw_f32_le"
    case zstdF32LE = "zstd_f32_le"
}

public struct FightboxCanonicalChunkRecord: Codable, Sendable, Equatable {
    public let index: UInt32
    public let startFrame: UInt64
    public let frameCount: UInt32
    public let file: String
    public let encoding: FightboxCanonicalChunkEncoding
    public let uncompressedBytes: UInt64
    public let storedBytes: UInt64
    public let rawSHA256: String
    public let storedSHA256: String

    enum CodingKeys: String, CodingKey {
        case index
        case startFrame = "start_frame"
        case frameCount = "frame_count"
        case file, encoding
        case uncompressedBytes = "uncompressed_bytes"
        case storedBytes = "stored_bytes"
        case rawSHA256 = "raw_sha256"
        case storedSHA256 = "stored_sha256"
    }
}

public struct FightboxCanonicalSourceAsset: Codable, Sendable, Equatable {
    public struct CanonicalArtifact: Codable, Sendable, Equatable {
        public let artifactID: String
        public let canonicalPCMSHA256: String
        public let frameCount: UInt64
        public let chunkFrames: UInt32

        enum CodingKeys: String, CodingKey {
            case artifactID = "artifact_id"
            case canonicalPCMSHA256 = "canonical_pcm_sha256"
            case frameCount = "frame_count"
            case chunkFrames = "chunk_frames"
        }
    }

    public struct Derivation: Codable, Sendable, Equatable {
        public let sourceArtifactID: String
        public let recipeSHA256: String

        enum CodingKeys: String, CodingKey {
            case sourceArtifactID = "source_artifact_id"
            case recipeSHA256 = "recipe_sha256"
        }
    }

    public let schemaVersion: String
    public let assetID: String
    public let layout: FightboxCanonicalAssetLayout
    public let presentationProvenance: FightboxCanonicalPresentationProvenance
    public let compatibleGeometries: [FightboxCanonicalSourceGeometry]
    public let canonical: CanonicalArtifact
    public let derivation: Derivation?

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case assetID = "asset_id"
        case layout
        case presentationProvenance = "presentation_provenance"
        case compatibleGeometries = "compatible_geometries"
        case canonical, derivation
    }
}

public struct FightboxCanonicalAudioManifest: Codable, Sendable, Equatable {
    public let schemaVersion: String
    public let sourceDescriptorSHA256: String
    public let sourceAsset: FightboxCanonicalSourceAsset
    public let sampleRateHz: UInt32
    public let channelCount: UInt16
    public let frameCount: UInt64
    public let chunkFrames: UInt32
    public let compressorRevision: String
    public let chunks: [FightboxCanonicalChunkRecord]

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case sourceDescriptorSHA256 = "source_descriptor_sha256"
        case sourceAsset = "source_asset"
        case sampleRateHz = "sample_rate_hz"
        case channelCount = "channel_count"
        case frameCount = "frame_count"
        case chunkFrames = "chunk_frames"
        case compressorRevision = "compressor_revision"
        case chunks
    }
}

public struct FightboxCanonicalAssetMemory: Sendable, Equatable {
    /// Stable PCM storage borrowed by the callback. MonoExpanded is counted as
    /// two delivered planes even though its canonical package stores one.
    public let decodedResidentBytes: Int
    /// Largest expected worker-side overlap of stored bytes, raw bytes,
    /// decoded floats, and MonoExpanded history/output while filling one slot.
    public let maximumEstimatedLoaderScratchBytes: Int
    public let cacheSeconds: Int
    public let deliveredProgramPlaneCount: Int
}

public struct FightboxCanonicalAssetStatus: Sendable, Equatable {
    public let assetID: String
    public let artifactID: String
    public let sourceIndex: Int
    public let currentOriginalTimelineFrame: UInt64
    public let frameCount: UInt64
    public let residentChunkCount: Int
    public let underrunCount: UInt64
    public let active: Bool
    public let memory: FightboxCanonicalAssetMemory
}

public struct FightboxMacroAssetArrival: Sendable, Equatable {
    /// Exact source-program position emitted by `ScheduledMacroEvent` or by the
    /// continuous macro transport plan after converting seconds to 48 kHz.
    public let programSeekFrame: UInt64

    public init(programSeekFrame: UInt64) {
        self.programSeekFrame = programSeekFrame
    }

    /// Converts macro `program_seek_s` to the original canonical timeline.
    /// The clamp is only for finite representability; no propagation delay is
    /// reapplied here because macro transport has already removed it.
    public init(programSeekSeconds: Double) throws {
        let frames = programSeekSeconds * Double(canonicalSampleRate)
        guard frames.isFinite, frames >= 0, frames <= Double(UInt64.max) else {
            throw FightboxCanonicalAssetError.invalidMacroSeek(programSeekSeconds)
        }
        programSeekFrame = UInt64(frames.rounded())
    }
}

/// Exact resident asset interval requested for one macro callback block.
/// The destination offset preserves sample-accurate mid-block onset; commit
/// advances only `frameCount`, never the provider's full callback size.
public struct FightboxMacroProgramInterval: Sendable, Equatable {
    public let assetFrameStart: UInt64
    public let frameCount: Int
    public let destinationFrameOffset: Int

    public init(
        assetFrameStart: UInt64,
        frameCount: Int,
        destinationFrameOffset: Int
    ) {
        self.assetFrameStart = assetFrameStart
        self.frameCount = frameCount
        self.destinationFrameOffset = destinationFrameOffset
    }
}

/// Immutable control-side association between a compact Rust macro asset key
/// and one preallocated canonical playback occupying an engine-owned role slot.
@available(macOS 10.15, *)
public struct FightboxMacroAssetBinding: @unchecked Sendable {
    public let assetKey: UInt64
    public let playback: FightboxCanonicalAssetPlayback

    public init(assetKey: UInt64, playback: FightboxCanonicalAssetPlayback) {
        precondition(assetKey != 0)
        precondition((12 ... 15).contains(playback.sourceIndex))
        self.assetKey = assetKey
        self.playback = playback
    }
}

/// Token returned only after an exact seek is resident and callback-readable.
public struct FightboxMacroAssetReadiness: Sendable, Equatable {
    public let assetKey: UInt64
    public let sourceIndex: Int
    public let programSeekFrame: UInt64
    public let discontinuitySequence: UInt64

    public init(
        assetKey: UInt64,
        sourceIndex: Int,
        programSeekFrame: UInt64,
        discontinuitySequence: UInt64
    ) {
        self.assetKey = assetKey
        self.sourceIndex = sourceIndex
        self.programSeekFrame = programSeekFrame
        self.discontinuitySequence = discontinuitySequence
    }
}

/// One callback interval correlated with its control-side readiness token.
public struct FightboxMacroProgramRequest: Sendable, Equatable {
    public let readiness: FightboxMacroAssetReadiness
    public let interval: FightboxMacroProgramInterval

    public init(
        readiness: FightboxMacroAssetReadiness,
        interval: FightboxMacroProgramInterval
    ) {
        self.readiness = readiness
        self.interval = interval
    }
}

/// Named source choices intentionally contain metadata only. Applications map
/// these contracts to licensed local canonical packages; FightboxKit ships no
/// copyrighted Tom's Diner bytes.
public struct FightboxNamedSourceContract: Sendable, Equatable, Identifiable {
    public let id: String
    public let displayName: String
    public let expectedLayout: FightboxCanonicalAssetLayout
    public let expectedProvenance: FightboxCanonicalPresentationProvenance
    public let notes: String

    public static let tomsDinerMonoReference = FightboxNamedSourceContract(
        id: "toms-diner",
        displayName: "Tom's Diner · mono reference",
        expectedLayout: .mono,
        expectedProvenance: .nativeMono,
        notes: "Existing regression identity; user supplies the licensed canonical package."
    )

    public static let tomsDinerAuthoredStereo = FightboxNamedSourceContract(
        id: "toms-diner-authored-stereo",
        displayName: "Tom's Diner · authored stereo",
        expectedLayout: .stereoLR,
        expectedProvenance: .authoredStereo,
        notes: "True L/R presentation; no downmix, widening, or bundled media."
    )

    public static let tomsDinerChoices = [
        tomsDinerMonoReference,
        tomsDinerAuthoredStereo,
    ]
}

public enum FightboxCanonicalAssetError: Error, Sendable, CustomStringConvertible {
    case cannotReadManifest(String)
    case invalidManifest(String)
    case incompatibleProgram(String)
    case cannotReadChunk(String)
    case chunkIdentityMismatch(index: UInt32, phase: String)
    case zstdDecompressionFailed(index: UInt32)
    case nonFinitePCM(index: UInt32)
    case invalidSeek(frame: UInt64, frameCount: UInt64)
    case invalidMacroSeek(Double)
    case macroAssetNotBound(UInt64)
    case macroAssetNotReady(UInt64)
    case allocationFailed

    public var description: String {
        switch self {
        case let .cannotReadManifest(message): return "Cannot read canonical manifest: \(message)"
        case let .invalidManifest(message): return "Invalid canonical manifest: \(message)"
        case let .incompatibleProgram(message): return "Canonical asset/program mismatch: \(message)"
        case let .cannotReadChunk(message): return "Cannot read canonical chunk: \(message)"
        case let .chunkIdentityMismatch(index, phase):
            return "Canonical chunk \(index) \(phase)-byte identity mismatch"
        case let .zstdDecompressionFailed(index):
            return "Canonical chunk \(index) zstd decompression failed"
        case let .nonFinitePCM(index): return "Canonical chunk \(index) contains non-finite PCM"
        case let .invalidSeek(frame, frameCount):
            return "Canonical seek frame \(frame) exceeds source length \(frameCount)"
        case let .invalidMacroSeek(seconds): return "Invalid macro program seek \(seconds) seconds"
        case let .macroAssetNotBound(assetKey):
            return "Macro asset key \(assetKey) is not bound to a reserved playback slot"
        case let .macroAssetNotReady(assetKey):
            return "Macro asset key \(assetKey) did not publish exact resident readiness"
        case .allocationFailed: return "Cannot allocate canonical playback state"
        }
    }
}

/// One source's asynchronous canonical package and fixed decoded cache.
///
/// Disk reads, hashes, zstd, and MonoExpanded derivation stay on `ioQueue`.
/// `copyNextBlock` only performs atomic loads and memcpy into the neutral bank.
@available(macOS 10.15, *)
public final class FightboxCanonicalAssetPlayback: @unchecked Sendable {
    public let packageURL: URL
    public let sourceIndex: Int
    public let manifest: FightboxCanonicalAudioManifest
    public let memory: FightboxCanonicalAssetMemory

    private let cacheSeconds: Int
    private let outputPlaneCount: Int
    private let storage: UnsafeMutablePointer<Float>
    private let storageSampleCount: Int
    private let atomics: OpaquePointer
    private let ioQueue: DispatchQueue
    private var refillTimer: DispatchSourceTimer?
    /// Audio-thread-only staging truth for the provider's two-phase fill.
    private var callbackPendingStartFrame: UInt64?
    private var callbackPendingDiscontinuitySequence: UInt64 = 0
    private var callbackPendingAdvanceFrames: UInt64 = 0

    private init(
        packageURL: URL,
        sourceIndex: Int,
        expectedProgram: FightboxSpatialSourceProgram,
        cacheSeconds: Int
    ) throws {
        guard (2 ... 4).contains(cacheSeconds) else {
            throw FightboxCanonicalAssetError.invalidManifest(
                "runtime cache must contain 2...4 one-second chunks"
            )
        }
        let manifestURL = packageURL.appendingPathComponent("manifest.json")
        let data: Data
        do {
            data = try Data(contentsOf: manifestURL, options: [.mappedIfSafe])
        } catch {
            throw FightboxCanonicalAssetError.cannotReadManifest(error.localizedDescription)
        }
        do {
            manifest = try JSONDecoder().decode(FightboxCanonicalAudioManifest.self, from: data)
        } catch {
            throw FightboxCanonicalAssetError.cannotReadManifest(error.localizedDescription)
        }
        try Self.validate(manifest: manifest, expectedProgram: expectedProgram)

        self.packageURL = packageURL
        self.sourceIndex = sourceIndex
        self.cacheSeconds = cacheSeconds
        outputPlaneCount = manifest.sourceAsset.presentationProvenance.deliveredProgramPlaneCount
        storageSampleCount = cacheSeconds * outputPlaneCount * canonicalChunkFrames
        storage = .allocate(capacity: storageSampleCount)
        storage.initialize(repeating: 0, count: storageSampleCount)
        guard let mailbox = fb_canonical_playback_atomics_create() else {
            storage.deinitialize(count: storageSampleCount)
            storage.deallocate()
            throw FightboxCanonicalAssetError.allocationFailed
        }
        atomics = mailbox
        ioQueue = DispatchQueue(
            label: "fightbox.canonical-audio.\(sourceIndex)",
            qos: .userInitiated
        )

        let residentBytes = storageSampleCount * MemoryLayout<Float>.size
        let maximumRaw = manifest.chunks.map(\.uncompressedBytes).max() ?? 0
        let maximumStored = manifest.chunks.map(\.storedBytes).max() ?? 0
        let historyMultiplier = manifest.sourceAsset.presentationProvenance == .monoExpanded ? 4 : 2
        let scratch = Int(maximumStored) + Int(maximumRaw) * historyMultiplier
        memory = FightboxCanonicalAssetMemory(
            decodedResidentBytes: residentBytes,
            maximumEstimatedLoaderScratchBytes: scratch,
            cacheSeconds: cacheSeconds,
            deliveredProgramPlaneCount: outputPlaneCount
        )
    }

    deinit {
        refillTimer?.setEventHandler {}
        refillTimer?.cancel()
        fb_canonical_active_store(atomics, 0)
        fb_canonical_playback_atomics_destroy(atomics)
        storage.deinitialize(count: storageSampleCount)
        storage.deallocate()
    }

    public static func open(
        packageURL: URL,
        sourceIndex: Int,
        expectedProgram: FightboxSpatialSourceProgram,
        cacheSeconds: Int = 3,
        initialOriginalTimelineFrame: UInt64 = 0,
        active: Bool = true
    ) async throws -> FightboxCanonicalAssetPlayback {
        guard (0 ..< 16).contains(sourceIndex) else {
            throw FightboxCanonicalAssetError.incompatibleProgram(
                "source index \(sourceIndex) exceeds the 16-source runtime shape"
            )
        }
        return try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                do {
                    let playback = try FightboxCanonicalAssetPlayback(
                        packageURL: packageURL,
                        sourceIndex: sourceIndex,
                        expectedProgram: expectedProgram,
                        cacheSeconds: cacheSeconds
                    )
                    try playback.validateSeek(initialOriginalTimelineFrame)
                    try playback.loadWorkingSet(around: initialOriginalTimelineFrame)
                    fb_canonical_playhead_store(playback.atomics, initialOriginalTimelineFrame)
                    fb_canonical_active_store(playback.atomics, active ? 1 : 0)
                    playback.startRefillTimer()
                    continuation.resume(returning: playback)
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }

    public func setActive(_ active: Bool) {
        fb_canonical_active_store(atomics, active ? 1 : 0)
    }

    /// Discontinuously seeks in the original canonical asset timeline. The
    /// source is silent while the new working set is loaded and published.
    public func seek(toOriginalTimelineFrame frame: UInt64) async throws {
        try validateSeek(frame)
        let resumeActive = fb_canonical_active_load(atomics) != 0
        fb_canonical_active_store(atomics, 0)
        do {
            try await runOnIOQueue { [self] in
                try loadWorkingSet(around: frame)
                fb_canonical_publish_seek(atomics, frame)
            }
            fb_canonical_active_store(atomics, resumeActive ? 1 : 0)
        } catch {
            fb_canonical_active_store(atomics, resumeActive ? 1 : 0)
            throw error
        }
    }

    /// Uses macro transport's already propagation-adjusted program position.
    /// A 10 km source therefore begins roughly 29.15 seconds later in wall
    /// time, but at the original asset frame represented by this value.
    public func seekForMacroArrival(_ arrival: FightboxMacroAssetArrival) async throws {
        try await seek(toOriginalTimelineFrame: arrival.programSeekFrame)
    }

    public func status() -> FightboxCanonicalAssetStatus {
        var resident = 0
        for slot in 0 ..< cacheSeconds
        where fb_canonical_slot_chunk_index(atomics, UInt32(slot)) != canonicalInvalidChunk {
            resident += 1
        }
        return FightboxCanonicalAssetStatus(
            assetID: manifest.sourceAsset.assetID,
            artifactID: manifest.sourceAsset.canonical.artifactID,
            sourceIndex: sourceIndex,
            currentOriginalTimelineFrame: fb_canonical_playhead_load(atomics),
            frameCount: manifest.frameCount,
            residentChunkCount: resident,
            underrunCount: fb_canonical_underrun_count(atomics),
            active: fb_canonical_active_load(atomics) != 0,
            memory: memory
        )
    }

    public var discontinuitySequence: UInt64 {
        fb_canonical_discontinuity_load(atomics)
    }

    fileprivate func stageNextBlock(into bank: FightboxSpatialProgramBank) {
        discardStagedBlock()
        guard fb_canonical_active_load(atomics) != 0 else { return }
        let startFrame = fb_canonical_playhead_load(atomics)
        let discontinuity = fb_canonical_discontinuity_load(atomics)
        guard discontinuity & 1 == 0 else { return }
        if startFrame >= manifest.frameCount {
            fb_canonical_active_store(atomics, 0)
            return
        }

        let requestedFrames = bank.blockSizeFrames
        let availableFrames = Int(min(
            UInt64(requestedFrames),
            manifest.frameCount - startFrame
        ))
        guard copyResidentInterval(
            startFrame: startFrame,
            frameCount: availableFrames,
            destinationFrameOffset: 0,
            into: bank
        ) else {
            clearSource(in: bank)
            fb_canonical_record_underrun(atomics)
            return
        }
        callbackPendingStartFrame = startFrame
        callbackPendingDiscontinuitySequence = discontinuity
        callbackPendingAdvanceFrames = UInt64(requestedFrames)
    }

    /// Stages one exact macro interval already made resident by the control-side
    /// seek/prefetch path. Audio-thread only: no I/O, decode, allocation, or lock.
    /// A failure is non-advancing and must prevent the enclosing render commit.
    public func stageMacroInterval(
        _ interval: FightboxMacroProgramInterval,
        into bank: FightboxSpatialProgramBank
    ) -> OSStatus {
        discardStagedBlock()
        guard sourceIndex >= 0,
              sourceIndex < bank.sourceCount,
              interval.frameCount > 0,
              interval.destinationFrameOffset >= 0,
              interval.destinationFrameOffset <= bank.blockSizeFrames,
              interval.frameCount <= bank.blockSizeFrames - interval.destinationFrameOffset,
              fb_canonical_active_load(atomics) != 0
        else {
            return kAudio_ParamError
        }
        let discontinuity = fb_canonical_discontinuity_load(atomics)
        let endFrame = interval.assetFrameStart.addingReportingOverflow(
            UInt64(interval.frameCount)
        )
        guard discontinuity & 1 == 0,
              fb_canonical_playhead_load(atomics) == interval.assetFrameStart,
              !endFrame.overflow,
              endFrame.partialValue <= manifest.frameCount
        else {
            return kAudioUnitErr_CannotDoInCurrentContext
        }
        guard copyResidentInterval(
            startFrame: interval.assetFrameStart,
            frameCount: interval.frameCount,
            destinationFrameOffset: interval.destinationFrameOffset,
            into: bank
        ) else {
            clearSource(in: bank)
            fb_canonical_record_underrun(atomics)
            return kAudioUnitErr_CannotDoInCurrentContext
        }
        callbackPendingStartFrame = interval.assetFrameStart
        callbackPendingDiscontinuitySequence = discontinuity
        callbackPendingAdvanceFrames = UInt64(interval.frameCount)
        return noErr
    }

    private func copyResidentInterval(
        startFrame: UInt64,
        frameCount: Int,
        destinationFrameOffset: Int,
        into bank: FightboxSpatialProgramBank
    ) -> Bool {
        var copiedFrames = 0
        while copiedFrames < frameCount {
            let absoluteFrame = startFrame + UInt64(copiedFrames)
            let chunkIndex = Int(absoluteFrame / UInt64(canonicalChunkFrames))
            let offset = Int(absoluteFrame % UInt64(canonicalChunkFrames))
            let run = min(frameCount - copiedFrames, canonicalChunkFrames - offset)
            guard let slot = residentSlot(for: UInt64(chunkIndex)) else {
                return false
            }
            let sequence = fb_canonical_slot_sequence(atomics, UInt32(slot))
            guard sequence & 1 == 0 else {
                return false
            }
            for channel in 0 ..< outputPlaneCount {
                bank.withMutableChannel(sourceIndex: sourceIndex, channel: channel) { destination in
                    let source = slotPlane(slot: slot, channel: channel).advanced(by: offset)
                    destination.baseAddress?
                        .advanced(by: destinationFrameOffset + copiedFrames)
                        .update(from: source, count: run)
                }
            }
            guard sequence == fb_canonical_slot_sequence(atomics, UInt32(slot)),
                  fb_canonical_slot_chunk_index(atomics, UInt32(slot)) == UInt64(chunkIndex)
            else {
                return false
            }
            copiedFrames += run
        }
        return true
    }

    fileprivate func commitStagedBlock() {
        guard let startFrame = callbackPendingStartFrame,
              callbackPendingAdvanceFrames > 0,
              callbackPendingDiscontinuitySequence ==
                  fb_canonical_discontinuity_load(atomics)
        else {
            discardStagedBlock()
            return
        }
        let remaining = manifest.frameCount - startFrame
        let desired = callbackPendingAdvanceFrames >= remaining
            ? manifest.frameCount
            : startFrame + callbackPendingAdvanceFrames
        let committed = fb_canonical_playhead_commit(
            atomics,
            startFrame,
            desired,
            callbackPendingDiscontinuitySequence
        ) != 0
        let stillSameDiscontinuity = callbackPendingDiscontinuitySequence ==
            fb_canonical_discontinuity_load(atomics)
        if committed, stillSameDiscontinuity, desired >= manifest.frameCount {
            fb_canonical_active_store(atomics, 0)
        }
        discardStagedBlock()
    }

    fileprivate func discardStagedBlock() {
        callbackPendingStartFrame = nil
        callbackPendingDiscontinuitySequence = 0
        callbackPendingAdvanceFrames = 0
    }

    private func clearSource(in bank: FightboxSpatialProgramBank) {
        for channel in 0 ..< outputPlaneCount {
            bank.withMutableChannel(sourceIndex: sourceIndex, channel: channel) { buffer in
                buffer.update(repeating: 0)
            }
        }
    }

    private func residentSlot(for chunkIndex: UInt64) -> Int? {
        for slot in 0 ..< cacheSeconds
        where fb_canonical_slot_chunk_index(atomics, UInt32(slot)) == chunkIndex {
            return slot
        }
        return nil
    }

    private func slotPlane(slot: Int, channel: Int) -> UnsafeMutablePointer<Float> {
        storage.advanced(
            by: (slot * outputPlaneCount + channel) * canonicalChunkFrames
        )
    }

    private func validateSeek(_ frame: UInt64) throws {
        guard frame <= manifest.frameCount else {
            throw FightboxCanonicalAssetError.invalidSeek(
                frame: frame,
                frameCount: manifest.frameCount
            )
        }
    }

    private func startRefillTimer() {
        let timer = DispatchSource.makeTimerSource(queue: ioQueue)
        timer.schedule(deadline: .now() + .milliseconds(50), repeating: .milliseconds(50))
        timer.setEventHandler { [weak self] in
            guard let self, fb_canonical_active_load(atomics) != 0 else { return }
            try? loadWorkingSet(around: fb_canonical_playhead_load(atomics))
        }
        refillTimer = timer
        timer.resume()
    }

    private func runOnIOQueue(_ body: @escaping @Sendable () throws -> Void) async throws {
        try await withCheckedThrowingContinuation { continuation in
            ioQueue.async {
                do {
                    try body()
                    continuation.resume()
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }

    private func loadWorkingSet(around frame: UInt64) throws {
        guard frame < manifest.frameCount else { return }
        let first = Int(frame / UInt64(canonicalChunkFrames))
        let upper = min(first + cacheSeconds, manifest.chunks.count)
        for chunkIndex in first ..< upper {
            if residentSlot(for: UInt64(chunkIndex)) == nil {
                try loadPresentationChunk(index: chunkIndex)
            }
        }
    }

    private func loadPresentationChunk(index: Int) throws {
        let slot = index % cacheSeconds
        fb_canonical_slot_begin_write(atomics, UInt32(slot))
        do {
            let source = try decodeSourceChunk(index: index)
            let presentation: [[Float]]
            switch manifest.sourceAsset.presentationProvenance {
            case .nativeMono, .authoredStereo:
                presentation = source
            case .monoExpanded:
                presentation = try expandMono(source[0], chunkIndex: index)
            }
            for channel in 0 ..< outputPlaneCount {
                let destination = slotPlane(slot: slot, channel: channel)
                destination.update(repeating: 0, count: canonicalChunkFrames)
                presentation[channel].withUnsafeBufferPointer { source in
                    guard let base = source.baseAddress else { return }
                    destination.update(from: base, count: source.count)
                }
            }
            fb_canonical_slot_publish(atomics, UInt32(slot), UInt64(index))
        } catch {
            fb_canonical_slot_invalidate(atomics, UInt32(slot))
            throw error
        }
    }

    private func decodeSourceChunk(index: Int) throws -> [[Float]] {
        let record = manifest.chunks[index]
        let chunkURL = packageURL.appendingPathComponent(record.file)
        let stored: Data
        do {
            stored = try Data(contentsOf: chunkURL, options: [.mappedIfSafe])
        } catch {
            throw FightboxCanonicalAssetError.cannotReadChunk(error.localizedDescription)
        }
        guard UInt64(stored.count) == record.storedBytes,
              Self.sha256(stored) == record.storedSHA256
        else {
            throw FightboxCanonicalAssetError.chunkIdentityMismatch(
                index: record.index,
                phase: "stored"
            )
        }

        let raw: Data
        switch record.encoding {
        case .rawF32LE:
            raw = stored
        case .zstdF32LE:
            var decoded = Data(count: Int(record.uncompressedBytes))
            let status = decoded.withUnsafeMutableBytes { destination in
                stored.withUnsafeBytes { source in
                    fb_canonical_zstd_decompress_exact(
                        destination.baseAddress,
                        destination.count,
                        source.baseAddress,
                        source.count
                    )
                }
            }
            guard status == 0 else {
                throw FightboxCanonicalAssetError.zstdDecompressionFailed(index: record.index)
            }
            raw = decoded
        }
        guard UInt64(raw.count) == record.uncompressedBytes,
              Self.sha256(raw) == record.rawSHA256
        else {
            throw FightboxCanonicalAssetError.chunkIdentityMismatch(
                index: record.index,
                phase: "raw"
            )
        }

        let channels = Int(manifest.channelCount)
        let frames = Int(record.frameCount)
        var planar = Array(
            repeating: [Float](repeating: 0, count: frames),
            count: channels
        )
        raw.withUnsafeBytes { bytes in
            for channel in 0 ..< channels {
                planar[channel].withUnsafeMutableBufferPointer { output in
                    let source = bytes.baseAddress!.advanced(by: channel * frames * 4)
                    output.baseAddress!.update(
                        from: source.assumingMemoryBound(to: Float.self),
                        count: frames
                    )
                }
            }
        }
        guard planar.joined().allSatisfy(\.isFinite) else {
            throw FightboxCanonicalAssetError.nonFinitePCM(index: record.index)
        }
        return planar
    }

    private func expandMono(_ current: [Float], chunkIndex: Int) throws -> [[Float]] {
        let previous = chunkIndex > 0 ? try decodeSourceChunk(index: chunkIndex - 1)[0] : []
        let chunkStart = UInt64(chunkIndex * canonicalChunkFrames)
        var left = [Float](repeating: 0, count: current.count)
        var right = [Float](repeating: 0, count: current.count)
        for offset in current.indices {
            let absolute = chunkStart + UInt64(offset)
            let center = current[offset]
            let near = delayedMono(
                current: current,
                previous: previous,
                chunkStart: chunkStart,
                absoluteFrame: absolute,
                delay: monoExpansionNearDelay
            )
            let far = delayedMono(
                current: current,
                previous: previous,
                chunkStart: chunkStart,
                absoluteFrame: absolute,
                delay: monoExpansionFarDelay
            )
            let side = (Double(near) - Double(far)) * monoExpansionSideGain
            (left[offset], right[offset]) = Self.exactMonoPair(center: center, side: side)
        }
        return [left, right]
    }

    private func delayedMono(
        current: [Float],
        previous: [Float],
        chunkStart: UInt64,
        absoluteFrame: UInt64,
        delay: Int
    ) -> Float {
        guard absoluteFrame >= UInt64(delay) else { return 0 }
        let delayed = absoluteFrame - UInt64(delay)
        if delayed >= chunkStart {
            return current[Int(delayed - chunkStart)]
        }
        guard !previous.isEmpty else { return 0 }
        let previousStart = chunkStart - UInt64(canonicalChunkFrames)
        return previous[Int(delayed - previousStart)]
    }

    private static func exactMonoPair(center: Float, side requestedSide: Double) -> (Float, Float) {
        guard center != 0 else { return (center, center) }
        var side = requestedSide
        let widenedCenter = Double(center)
        for _ in 0 ..< 32 {
            let left = Float(widenedCenter + side)
            let right = Float(widenedCenter - side)
            let f32Fold = fmaf(left, 0.5, right * 0.5)
            let f64Fold = Float((Double(left) + Double(right)) * 0.5)
            if left.isFinite, right.isFinite,
               f32Fold.bitPattern == center.bitPattern,
               f64Fold.bitPattern == center.bitPattern {
                return (left, right)
            }
            side *= 0.5
        }
        return (center, center)
    }

    private static func sha256(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }

    private static func validate(
        manifest: FightboxCanonicalAudioManifest,
        expectedProgram: FightboxSpatialSourceProgram
    ) throws {
        guard manifest.schemaVersion == canonicalPackageSchema,
              manifest.sourceAsset.schemaVersion == canonicalSourceSchema,
              manifest.sampleRateHz == canonicalSampleRate,
              manifest.chunkFrames == UInt32(canonicalChunkFrames),
              manifest.compressorRevision == canonicalCompressorRevision,
              manifest.channelCount == UInt16(manifest.sourceAsset.layout.storedChannelCount),
              manifest.frameCount == manifest.sourceAsset.canonical.frameCount,
              manifest.sourceAsset.canonical.chunkFrames == UInt32(canonicalChunkFrames),
              manifest.sourceDescriptorSHA256.count == 64
        else {
            throw FightboxCanonicalAssetError.invalidManifest(
                "unsupported schema, rate, chunking, compressor, or channel contract"
            )
        }
        let expectedChunkCount = Int(
            (manifest.frameCount + UInt64(canonicalChunkFrames) - 1)
                / UInt64(canonicalChunkFrames)
        )
        guard manifest.chunks.count == expectedChunkCount else {
            throw FightboxCanonicalAssetError.invalidManifest("chunk count does not cover asset")
        }
        for (index, chunk) in manifest.chunks.enumerated() {
            let start = UInt64(index * canonicalChunkFrames)
            let frames = min(UInt64(canonicalChunkFrames), manifest.frameCount - start)
            let safeComponents = NSString(string: chunk.file).pathComponents
            guard chunk.index == UInt32(index),
                  chunk.startFrame == start,
                  chunk.frameCount == UInt32(frames),
                  chunk.file.hasPrefix(String(format: "chunks/%08d.", index)),
                  !chunk.file.hasPrefix("/"),
                  !safeComponents.contains(".."),
                  chunk.uncompressedBytes == frames * UInt64(manifest.channelCount) * 4,
                  chunk.rawSHA256.count == 64,
                  chunk.storedSHA256.count == 64
            else {
                throw FightboxCanonicalAssetError.invalidManifest(
                    "chunk \(index) violates index, range, path, size, or identity shape"
                )
            }
        }

        let source = manifest.sourceAsset
        let expectedPlanes = source.presentationProvenance.deliveredProgramPlaneCount
        guard expectedProgram.channelCount == UInt32(expectedPlanes) else {
            throw FightboxCanonicalAssetError.incompatibleProgram(
                "source requires \(expectedPlanes) program planes, session configured \(expectedProgram.channelCount)"
            )
        }
        let geometry: FightboxCanonicalSourceGeometry
        switch expectedProgram.geometry {
        case .point: geometry = .point
        case .multiPoint: geometry = .multiPoint
        case .lineSegment: geometry = .lineSegment
        case .stereoImage: geometry = .stereoImage
        }
        guard source.compatibleGeometries.contains(geometry) else {
            throw FightboxCanonicalAssetError.incompatibleProgram(
                "geometry \(geometry.rawValue) is not admitted by the source contract"
            )
        }
        switch (source.layout, source.presentationProvenance) {
        case (.mono, .nativeMono), (.stereoLR, .authoredStereo):
            guard source.derivation == nil else {
                throw FightboxCanonicalAssetError.invalidManifest(
                    "native/authored source must not carry a derivation"
                )
            }
        case (.mono, .monoExpanded):
            guard source.derivation?.sourceArtifactID == source.canonical.artifactID,
                  source.derivation?.recipeSHA256 == monoExpansionRecipeSHA256
            else {
                throw FightboxCanonicalAssetError.invalidManifest(
                    "MonoExpanded source does not identify the frozen absolute-frame recipe"
                )
            }
        default:
            throw FightboxCanonicalAssetError.invalidManifest(
                "layout and presentation provenance are incompatible"
            )
        }
    }
}

/// Immutable callback topology for one or more independently cached assets.
/// Bindings are sorted once and retained until the provider is released, so
/// callback staging performs only bounded scans, atomics, and resident copies.
@available(macOS 10.15, *)
public final class FightboxCanonicalProgramProvider:
    FightboxTransactionalSpatialProgramProvider,
    @unchecked Sendable
{
    public let playbacks: [FightboxCanonicalAssetPlayback]
    public let macroAssetBindings: [FightboxMacroAssetBinding]

    private let macroSourceMask: UInt16

    public init(
        playbacks: [FightboxCanonicalAssetPlayback],
        macroAssetBindings: [FightboxMacroAssetBinding] = []
    ) {
        let sorted = playbacks.sorted { $0.sourceIndex < $1.sourceIndex }
        precondition(Set(sorted.map(\.sourceIndex)).count == sorted.count)
        let bindings = macroAssetBindings.sorted {
            $0.playback.sourceIndex < $1.playback.sourceIndex
        }
        precondition(Set(bindings.map(\.assetKey)).count == bindings.count)
        precondition(Set(bindings.map { $0.playback.sourceIndex }).count == bindings.count)
        for binding in bindings {
            precondition(sorted.contains { $0 === binding.playback })
        }
        self.playbacks = sorted
        self.macroAssetBindings = bindings
        macroSourceMask = bindings.reduce(UInt16(0)) { mask, binding in
            mask | (UInt16(1) << UInt16(binding.playback.sourceIndex))
        }
    }

    public var decodedResidentBytes: Int {
        playbacks.reduce(0) { $0 + $1.memory.decodedResidentBytes }
    }

    public var programDiscontinuitySequence: UInt64 {
        var combined = UInt64(0xcbf2_9ce4_8422_2325)
        for playback in playbacks where !isMacroSource(playback.sourceIndex) {
            // Macro slots carry their own even readiness generation through
            // the V3 token and must not stall the whole engine callback clock
            // when they seek pre-due.
            combined ^= playback.discontinuitySequence &+
                UInt64(playback.sourceIndex &+ 1)
            combined = combined &* 0x0000_0100_0000_01b3
        }
        return combined
    }

    /// Resolves, seeks, and publishes one fixed macro slot before the event is
    /// due. Completion is the readiness boundary consumed by the tokened Rust
    /// activation commit; the callback never performs file or decode work.
    public func prepareMacroAsset(
        assetKey: UInt64,
        programSeekFrame: UInt64
    ) async throws -> FightboxMacroAssetReadiness {
        guard let binding = macroBinding(assetKey: assetKey) else {
            throw FightboxCanonicalAssetError.macroAssetNotBound(assetKey)
        }
        binding.playback.setActive(false)
        do {
            try await binding.playback.seekForMacroArrival(
                FightboxMacroAssetArrival(programSeekFrame: programSeekFrame)
            )
            binding.playback.setActive(true)
            let status = binding.playback.status()
            let discontinuity = binding.playback.discontinuitySequence
            guard status.active,
                  status.currentOriginalTimelineFrame == programSeekFrame,
                  programSeekFrame < status.frameCount,
                  status.residentChunkCount > 0,
                  discontinuity & 1 == 0
            else {
                binding.playback.setActive(false)
                throw FightboxCanonicalAssetError.macroAssetNotReady(assetKey)
            }
            return FightboxMacroAssetReadiness(
                assetKey: assetKey,
                sourceIndex: binding.playback.sourceIndex,
                programSeekFrame: programSeekFrame,
                discontinuitySequence: discontinuity
            )
        } catch {
            binding.playback.setActive(false)
            throw error
        }
    }

    /// Control-side release after the matching audio acknowledgement. A stale
    /// readiness token cannot deactivate a successor in the same role slot.
    @discardableResult
    public func releaseMacroAsset(_ readiness: FightboxMacroAssetReadiness) -> Bool {
        guard let binding = macroBinding(assetKey: readiness.assetKey),
              binding.playback.sourceIndex == readiness.sourceIndex,
              binding.playback.discontinuitySequence == readiness.discontinuitySequence
        else {
            return false
        }
        binding.playback.setActive(false)
        return true
    }

    /// Legacy provider behavior stays unchanged for unbound sources. Bound
    /// engine-owned macro slots are silent unless the callback supplies exact,
    /// readiness-correlated requests through `fillMacroIntervals`.
    public func fill(_ bank: FightboxSpatialProgramBank) -> OSStatus {
        bank.clear()
        for playback in playbacks {
            if isMacroSource(playback.sourceIndex) {
                playback.discardStagedBlock()
            } else {
                playback.stageNextBlock(into: bank)
            }
        }
        return noErr
    }

    /// Stages a complete callback transaction: ordinary sources receive their
    /// next block while requested macro slots copy only the exact resident
    /// intervals (including mid-block offsets). Validation happens before any
    /// playhead can commit; failure discards all staged state and clears PCM.
    public func fillMacroIntervals(
        _ requests: UnsafeBufferPointer<FightboxMacroProgramRequest>,
        into bank: FightboxSpatialProgramBank
    ) -> OSStatus {
        guard requests.count <= macroAssetBindings.count else {
            return kAudio_ParamError
        }
        var requestedSourceMask = UInt16(0)
        for request in requests {
            guard let binding = macroBinding(assetKey: request.readiness.assetKey),
                  binding.playback.sourceIndex == request.readiness.sourceIndex,
                  binding.playback.discontinuitySequence ==
                    request.readiness.discontinuitySequence,
                  request.readiness.discontinuitySequence & 1 == 0
            else {
                return kAudioUnitErr_CannotDoInCurrentContext
            }
            let sourceBit = UInt16(1) << UInt16(binding.playback.sourceIndex)
            guard requestedSourceMask & sourceBit == 0 else {
                return kAudio_ParamError
            }
            requestedSourceMask |= sourceBit
        }

        bank.clear()
        for playback in playbacks {
            if isMacroSource(playback.sourceIndex) {
                playback.discardStagedBlock()
            } else {
                playback.stageNextBlock(into: bank)
            }
        }
        for request in requests {
            guard let binding = macroBinding(assetKey: request.readiness.assetKey) else {
                discardFilledBlock()
                bank.clear()
                return kAudioUnitErr_CannotDoInCurrentContext
            }
            let status = binding.playback.stageMacroInterval(request.interval, into: bank)
            guard status == noErr else {
                discardFilledBlock()
                bank.clear()
                return status
            }
        }
        return noErr
    }

    public func commitFilledBlock() {
        for playback in playbacks {
            playback.commitStagedBlock()
        }
    }

    public func discardFilledBlock() {
        for playback in playbacks {
            playback.discardStagedBlock()
        }
    }

    private func macroBinding(assetKey: UInt64) -> FightboxMacroAssetBinding? {
        for binding in macroAssetBindings where binding.assetKey == assetKey {
            return binding
        }
        return nil
    }

    private func isMacroSource(_ sourceIndex: Int) -> Bool {
        macroSourceMask & (UInt16(1) << UInt16(sourceIndex)) != 0
    }
}
