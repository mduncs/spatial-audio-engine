import Foundation

struct MotionSnapshot {
    let timestamp: Double
    let rawQuaternion: [Double]
    let rawYaw: Double
    let rawPitch: Double
    let rawRoll: Double

    var quaternion: MotionQuaternion? { MotionQuaternion.normalized(rawQuaternion) }

    func isFresh(at now: Double) -> Bool {
        timestamp.isFinite && timestamp >= 0 && timestamp <= now && now - timestamp <= 0.5
            && quaternion != nil && rawYaw.isFinite && rawPitch.isFinite && rawRoll.isFinite
    }
}

enum DirectionCheck: Equatable {
    case correct
    case failed(String)
    case inconclusive(String)
}

func checkLeft(_ forward: EnuForward, rotationRadians: Float) -> DirectionCheck {
    if rotationRadians < .pi / 9 { return .inconclusive("insufficient motion; turn LEFT about 90°") }
    if forward.east <= -0.65 && abs(forward.up) <= 0.5 && -forward.east >= abs(forward.north) {
        return .correct
    }
    if forward.east >= 0.65 && abs(forward.up) <= 0.5 && forward.east >= abs(forward.north) {
        return .failed("mirrored yaw; forward points EAST instead of WEST")
    }
    return .failed("wrong axis/direction; forward does not point WEST")
}

func checkUp(_ forward: EnuForward, rotationRadians: Float) -> DirectionCheck {
    if rotationRadians < .pi / 9 { return .inconclusive("insufficient motion; look farther UP") }
    if forward.up >= 0.35 && forward.up > abs(forward.east) { return .correct }
    if forward.up <= -0.35 { return .failed("wrong axis/direction; forward points DOWN instead of UP") }
    if abs(forward.east) >= 0.35 { return .failed("wrong axis; forward points SIDEWAYS instead of UP") }
    return .failed("wrong axis; head rotated but forward stays LEVEL (pitch interpreted as roll)")
}

struct GuidedProbe {
    enum Phase { case waiting, reference, left, recenter, up, finished }
    private(set) var phase = Phase.waiting
    private var phaseStarted: Double
    private var reference: MotionQuaternion?
    private var peak: (snapshot: MotionSnapshot, rotation: MotionQuaternion)?
    private var cue: String?
    private var failures: [String] = []
    private(set) var verdict: String?
    var succeeded: Bool { verdict == "Head tracking works and directions are correct" }

    init(started: Double) { phaseStarted = started }

    /// The latest instruction to speak; the user faces away from the screen.
    mutating func takeCue() -> String? {
        defer { cue = nil }
        return cue
    }

    mutating func tick(now: Double, authorized: Bool, available: Bool, failure: String?,
                       snapshot: MotionSnapshot?) -> [String] {
        if phase == .finished { return [] }
        if let failure { return finish(failure) }
        if phase == .waiting {
            if authorized && available && snapshot?.isFresh(at: now) == true {
                phase = .reference
                phaseStarted = now
                return [instruct("Face the screen and hold still.")]
            }
            if now - phaseStarted >= 20 {
                if !available { return finish("no headphones; connect and wear motion-capable AirPods, then rerun") }
                if !authorized { return finish("motion permission not granted; allow motion access, then rerun") }
                return finish("authorized and motion available, but no fresh valid motion records")
            }
            return []
        }
        if !authorized { return finish("motion authorization lost during guided check") }
        if !available { return finish("no headphones; disconnected during guided check") }
        guard let snapshot, snapshot.isFresh(at: now), let current = snapshot.quaternion else {
            return finish("stale or invalid motion during guided check")
        }
        if phase == .left || phase == .up, let reference {
            // Classify the farthest pose in the window, so turn timing does not matter.
            let rotation = coreMotionToEnu(reference: reference, current: current)
            if rotation.rotationRadians > peak?.rotation.rotationRadians ?? -1 { peak = (snapshot, rotation) }
        }
        if now - phaseStarted < Self.duration(phase) { return [] }
        phaseStarted = now
        switch phase {
        case .reference:
            reference = current
            phase = .left
            return [capture("Reference", snapshot, current),
                    instruct("Turn your head left, toward your left shoulder, and hold.")]
        case .left:
            let (best, rotation) = takePeak(snapshot, current)
            let result = summarize("LEFT", checkLeft(rotation.rotate(.north), rotationRadians: rotation.rotationRadians))
            phase = .recenter
            return [capture("LEFT peak", best, reference!), result, instruct("Face the screen again.")]
        case .recenter:
            phase = .up
            return [instruct("Now look up at the ceiling, and hold.")]
        case .up:
            let (best, rotation) = takePeak(snapshot, current)
            let result = summarize("UP", checkUp(rotation.rotate(.north), rotationRadians: rotation.rotationRadians))
            return [capture("UP peak", best, reference!), result] + finish(nil)
        case .waiting, .finished: return []
        }
    }

    private static func duration(_ phase: Phase) -> Double {
        switch phase {
        case .waiting: return 20
        case .reference, .recenter: return 3
        case .left, .up: return 5
        case .finished: return 0
        }
    }

    private mutating func takePeak(_ snapshot: MotionSnapshot, _ current: MotionQuaternion)
        -> (MotionSnapshot, MotionQuaternion) {
        defer { peak = nil }
        return peak ?? (snapshot, coreMotionToEnu(reference: reference!, current: current))
    }

    private mutating func instruct(_ text: String) -> String {
        cue = text
        return text
    }

    func liveLine(now: Double, snapshot: MotionSnapshot?) -> String {
        let label: String
        switch phase {
        case .waiting: label = "Waiting"
        case .reference, .recenter: label = "Face the screen"
        case .left: label = "LEFT check"
        case .up: label = "UP check"
        case .finished: return ""
        }
        let remaining = max(0, Int(ceil(Self.duration(phase) - (now - phaseStarted))))
        guard let snapshot else { return "\(label) \(remaining)s | waiting for fresh motion" }
        let scale = 180.0 / Double.pi
        return String(format: "%@ %ds | RAW yaw %+.1f° pitch %+.1f° roll %+.1f°", label, remaining,
                      snapshot.rawYaw * scale, snapshot.rawPitch * scale, snapshot.rawRoll * scale)
    }

    private func capture(_ label: String, _ snapshot: MotionSnapshot, _ reference: MotionQuaternion) -> String {
        let q = snapshot.rawQuaternion
        let forward = coreMotionToEnu(reference: reference, current: snapshot.quaternion!).rotate(.north)
        let scale = 180.0 / Double.pi
        return String(format: "%@: raw CM q w/x/y/z=[%+.6f,%+.6f,%+.6f,%+.6f]; RAW yaw/pitch/roll°=[%+.1f,%+.1f,%+.1f]; ENU forward east/north/up=[%+.4f,%+.4f,%+.4f]",
                      label, q[0], q[1], q[2], q[3], snapshot.rawYaw * scale, snapshot.rawPitch * scale,
                      snapshot.rawRoll * scale, Double(forward.east), Double(forward.north), Double(forward.up))
    }

    private mutating func summarize(_ label: String, _ check: DirectionCheck) -> String {
        switch check {
        case .correct: return "\(label) verified: forward points \(label == "LEFT" ? "WEST" : "UP")."
        case .failed(let reason):
            failures.append("\(label): \(reason)")
            return "\(label) failed: \(reason)."
        case .inconclusive(let reason):
            failures.append("\(label) inconclusive: \(reason)")
            return "\(label) inconclusive: \(reason)."
        }
    }

    private mutating func finish(_ reason: String?) -> [String] {
        if let reason { failures.append(reason) }
        phase = .finished
        verdict = failures.isEmpty ? "Head tracking works and directions are correct"
            : "Head tracking failed: " + failures.joined(separator: "; ")
        cue = failures.isEmpty ? "Done. Head tracking works." : "Done. Head tracking failed; see the screen."
        return [verdict!]
    }
}

private struct ProbeSelfTestError: Error, CustomStringConvertible {
    let description: String
}

func runSafeSelfTest() throws -> [String] {
    func require(_ condition: Bool, _ label: String) throws {
        if !condition { throw ProbeSelfTestError(description: label) }
    }
    guard let url = Bundle.main.url(forResource: "mapping-cases", withExtension: "tsv") else {
        throw ProbeSelfTestError(description: "missing bundled mapping-cases.tsv")
    }
    let text = try String(contentsOf: url, encoding: .utf8)
    var rawCases: [String: [Double]] = [:]
    var forwards: [String: EnuForward] = [:]
    var rotations: [String: MotionQuaternion] = [:]
    var lines: [String] = []
    for line in text.split(separator: "\n") where !line.hasPrefix("#") {
        let fields = line.split(separator: "\t")
        try require(fields.count == 12, "fixture must contain 12 TSV columns")
        let values = fields.dropFirst().compactMap { Double($0) }
        try require(values.count == 11, "invalid fixture number")
        guard let reference = MotionQuaternion.normalized(Array(values[0..<4])),
              let current = MotionQuaternion.normalized(Array(values[4..<8])) else {
            throw ProbeSelfTestError(description: "invalid fixture quaternion")
        }
        let rotation = coreMotionToEnu(reference: reference, current: current)
        let forward = rotation.rotate(.north)
        let expected = values[8..<11]
        try require(zip([forward.east, forward.north, forward.up], expected)
            .allSatisfy { abs(Double($0.0) - $0.1) < 1.0e-5 }, "mapping parity: \(fields[0])")
        let label = String(fields[0])
        rawCases[label] = Array(values[4..<8])
        forwards[label] = forward
        rotations[label] = rotation
        lines.append("Synthetic mapping parity PASS: \(label)")
    }
    try require(rawCases.count == 9, "expected nine shared mapping cases")
    try require(checkLeft(forwards["yaw-left"]!, rotationRadians: rotations["yaw-left"]!.rotationRadians) == .correct
        && checkUp(forwards["pitch-up"]!, rotationRadians: rotations["pitch-up"]!.rotationRadians) == .correct,
                "correct direction classification")
    try require(checkLeft(forwards["airpods-left-20261001"]!, rotationRadians: rotations["airpods-left-20261001"]!.rotationRadians) == .correct
        && checkUp(forwards["airpods-up-20261001"]!, rotationRadians: rotations["airpods-up-20261001"]!.rotationRadians) == .correct,
                "recorded AirPods probe classification")
    try require(checkLeft(forwards["yaw-mirrored"]!, rotationRadians: rotations["yaw-mirrored"]!.rotationRadians)
        == .failed("mirrored yaw; forward points EAST instead of WEST"),
                "mirrored yaw classification")
    try require(checkUp(forwards["pitch-mirrored"]!, rotationRadians: rotations["pitch-mirrored"]!.rotationRadians)
        == .failed("wrong axis/direction; forward points DOWN instead of UP"),
                "downward pitch classification")
    try require(checkUp(forwards["pitch-as-roll"]!, rotationRadians: rotations["pitch-as-roll"]!.rotationRadians)
        == .failed("wrong axis; head rotated but forward stays LEVEL (pitch interpreted as roll)"),
                "pitch-as-roll axis failure")
    func snapshot(_ raw: [Double], _ timestamp: Double) -> MotionSnapshot {
        MotionSnapshot(timestamp: timestamp, rawQuaternion: raw, rawYaw: 0.1, rawPitch: 0.2, rawRoll: 0.3)
    }
    func flow(_ left: [Double], _ up: [Double]) -> GuidedProbe {
        var probe = GuidedProbe(started: 0)
        let still: [Double] = [1, 0, 0, 0]
        // Each turn peaks mid-window; the head is back at the reference when its window closes.
        for (now, raw) in [(0.0, still), (3, still), (5, left), (8, still), (11, still), (13, up), (16, still)] {
            _ = probe.tick(now: now, authorized: true, available: true, failure: nil, snapshot: snapshot(raw, now))
        }
        return probe
    }
    try require(flow(rawCases["yaw-left"]!, rawCases["pitch-up"]!).succeeded, "normal synthetic guided flow")
    let mirrored = flow(rawCases["yaw-mirrored"]!, rawCases["pitch-mirrored"]!)
    try require(!mirrored.succeeded && mirrored.verdict!.contains("EAST") && mirrored.verdict!.contains("DOWN"),
                "mirrored synthetic guided flow")
    let swapped = flow(rawCases["pitch-up"]!, rawCases["yaw-left"]!)
    try require(!swapped.succeeded && swapped.verdict!.contains("SIDEWAYS"), "swapped synthetic guided flow")
    let pitchAsRoll = flow(rawCases["yaw-left"]!, rawCases["pitch-as-roll"]!)
    try require(!pitchAsRoll.succeeded && pitchAsRoll.verdict!.contains("LEVEL"), "pitch-as-roll synthetic guided flow")
    let insufficient = flow([1, 0, 0, 0], [1, 0, 0, 0])
    try require(!insufficient.succeeded && insufficient.verdict!.contains("inconclusive"), "insufficient synthetic guided flow")
    lines.append("Synthetic guided flows PASS: correct, mirrored, swapped axes, pitch-as-roll LEVEL, insufficient motion, peak capture")
    for (name, available, failure, time) in [("stale", true, nil as String?, 0.6),
                                           ("disconnect", false, nil, 0.1),
                                           ("error", true, "synthetic CoreMotion error", 0.1)] {
        var probe = GuidedProbe(started: 0)
        _ = probe.tick(now: 0, authorized: true, available: true, failure: nil, snapshot: snapshot([1, 0, 0, 0], 0))
        _ = probe.tick(now: time, authorized: true, available: available, failure: failure, snapshot: snapshot([1, 0, 0, 0], 0))
        try require(probe.phase == .finished && !probe.succeeded, "\(name) cannot pass a guided capture")
    }
    var waiting = GuidedProbe(started: 0)
    _ = waiting.tick(now: 20, authorized: false, available: true, failure: nil, snapshot: nil)
    try require(waiting.phase == .finished && !waiting.succeeded, "authorization wait timeout")
    try require(!snapshot([1, 0, 0, 0], 1).isFresh(at: 0), "future timestamp is invalid")
    lines.append("Synthetic freshness/device guards PASS: stale, disconnect, error, permission timeout, future timestamp")
    lines.append("Safe self-test passed: 9 shared mapping cases, including the recorded 2026-10-01 AirPods turns. Live hardware: run --probe.")
    return lines
}
