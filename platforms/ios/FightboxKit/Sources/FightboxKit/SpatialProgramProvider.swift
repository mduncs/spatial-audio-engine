import AudioToolbox
import Foundation

/// Supplies canonical program audio immediately before a neutral render.
/// Implementations run on the audio thread and must not allocate, lock, decode,
/// touch the filesystem, or perform control-side Fightbox calls.
public protocol FightboxSpatialProgramProvider: AnyObject, Sendable {
    func fill(_ bank: FightboxSpatialProgramBank) -> OSStatus
}

/// Optional two-phase callback contract used by seekable source programs.
///
/// `fill` stages one exact source-timeline block. The renderer commits it only
/// after the neutral backend returns a valid spatial block. Cell swaps, explicit
/// source seeks, and render failures discard it, so no inaudible 128-frame hole
/// is cut from the asset timeline.
public protocol FightboxTransactionalSpatialProgramProvider:
    FightboxSpatialProgramProvider
{
    var programDiscontinuitySequence: UInt64 { get }
    func commitFilledBlock()
    func discardFilledBlock()
}

/// Production-safe default used until an asset transport owns a source plane.
public final class FightboxSilentSpatialProgramProvider: FightboxTransactionalSpatialProgramProvider,
    @unchecked Sendable
{
    public init() {}

    public var programDiscontinuitySequence: UInt64 { 0 }

    public func fill(_ bank: FightboxSpatialProgramBank) -> OSStatus {
        bank.clear()
        return noErr
    }

    public func commitFilledBlock() {}
    public func discardFilledBlock() {}
}
