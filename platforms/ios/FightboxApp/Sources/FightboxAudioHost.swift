import AVFoundation
import AudioToolbox
import Foundation

/// Owns the preallocated buffers touched by the AVAudioSourceNode callback.
private final class FightboxBlockRenderer: @unchecked Sendable {
    private let session: FightboxSession
    private let sourceMajorMono: [Float]
    private var interleavedStereo: [Float]
    private var cachedFrameOffset: Int

    init(session: FightboxSession) {
        self.session = session
        sourceMajorMono = [Float](
            repeating: 0,
            count: session.sourceCount * session.blockSizeFrames
        )
        interleavedStereo = [Float](
            repeating: 0,
            count: session.blockSizeFrames * 2
        )
        cachedFrameOffset = session.blockSizeFrames
    }

    func render(
        frameCount: AVAudioFrameCount,
        audioBufferList: UnsafeMutablePointer<AudioBufferList>
    ) -> OSStatus {
        let buffers = UnsafeMutableAudioBufferListPointer(audioBufferList)
        for buffer in buffers where buffer.mData != nil {
            memset(buffer.mData, 0, Int(buffer.mDataByteSize))
        }

        let requestedFrames = Int(frameCount)
        var destinationOffset = 0
        while destinationOffset < requestedFrames {
            if cachedFrameOffset == session.blockSizeFrames {
                do {
                    try session.render(
                        sourceMajorMono: sourceMajorMono,
                        into: &interleavedStereo
                    )
                } catch {
                    cachedFrameOffset = session.blockSizeFrames
                    return kAudio_ParamError
                }
                cachedFrameOffset = 0
            }

            let framesToCopy = min(
                requestedFrames - destinationOffset,
                session.blockSizeFrames - cachedFrameOffset
            )
            copyFrames(
                from: cachedFrameOffset,
                count: framesToCopy,
                to: destinationOffset,
                buffers: buffers
            )
            cachedFrameOffset += framesToCopy
            destinationOffset += framesToCopy
        }
        return noErr
    }

    private func copyFrames(
        from sourceOffset: Int,
        count: Int,
        to destinationOffset: Int,
        buffers: UnsafeMutableAudioBufferListPointer
    ) {
        if buffers.count >= 2,
           let left = buffers[0].mData?.assumingMemoryBound(to: Float.self),
           let right = buffers[1].mData?.assumingMemoryBound(to: Float.self)
        {
            for frame in 0 ..< count {
                let sourceFrame = sourceOffset + frame
                left[destinationOffset + frame] = interleavedStereo[sourceFrame * 2]
                right[destinationOffset + frame] =
                    interleavedStereo[sourceFrame * 2 + 1]
            }
            return
        }

        guard buffers.count == 1,
              buffers[0].mNumberChannels == 2,
              let stereo = buffers[0].mData?.assumingMemoryBound(to: Float.self)
        else {
            return
        }
        for frame in 0 ..< count {
            let sourceFrame = sourceOffset + frame
            let destinationFrame = destinationOffset + frame
            stereo[destinationFrame * 2] = interleavedStereo[sourceFrame * 2]
            stereo[destinationFrame * 2 + 1] =
                interleavedStereo[sourceFrame * 2 + 1]
        }
    }
}

final class FightboxAudioHost {
    private static let sampleRate = 48_000.0
    private static let blockFrames = 128.0

    private let engine = AVAudioEngine()
    private let sourceNode: AVAudioSourceNode
    private let renderer: FightboxBlockRenderer

    init(session: FightboxSession) throws {
        guard let format = AVAudioFormat(
            standardFormatWithSampleRate: Self.sampleRate,
            channels: 2
        ) else {
            throw FightboxAudioHostError.cannotCreateFormat
        }

        renderer = FightboxBlockRenderer(session: session)
        sourceNode = AVAudioSourceNode(format: format) { [renderer] _, _, frames, buffers in
            renderer.render(frameCount: frames, audioBufferList: buffers)
        }
        engine.attach(sourceNode)
        engine.connect(sourceNode, to: engine.mainMixerNode, format: format)
        engine.prepare()
    }

    func start(monitorGainDB: Float) throws {
        let audioSession = AVAudioSession.sharedInstance()
        try audioSession.setCategory(.playback, mode: .default)
        try audioSession.setPreferredSampleRate(Self.sampleRate)
        try audioSession.setPreferredIOBufferDuration(
            Self.blockFrames / Self.sampleRate
        )
        try audioSession.setActive(true)
        setMonitorGainDB(monitorGainDB)
        try engine.start()
    }

    func stop() {
        engine.stop()
        try? AVAudioSession.sharedInstance().setActive(
            false,
            options: .notifyOthersOnDeactivation
        )
    }

    func setMonitorGainDB(_ gainDB: Float) {
        let clamped = min(max(gainDB, -60), 0)
        engine.mainMixerNode.outputVolume = powf(10, clamped / 20)
    }
}

enum FightboxAudioHostError: Error {
    case cannotCreateFormat
}

