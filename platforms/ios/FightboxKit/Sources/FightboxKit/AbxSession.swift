import Foundation

public let FIGHTBOX_ABX_SCHEMA_VERSION = "fightbox.abx.v1"
public let FIGHTBOX_ABX_MIN_TRIALS = 10
public let FIGHTBOX_ABX_REQUIRES_HUMAN =
    "Human on-device completion and sign-off are required; this template alone is not a pass."

public struct AbxListenerIdentity: Sendable, Equatable {
    public var listenerID: String
    public var notes: String

    public init(listenerID: String, notes: String = "") {
        self.listenerID = listenerID
        self.notes = notes
    }
}

public struct AbxHrtfRecord: Sendable, Equatable {
    public var hrtfSet: String
    public var pretestResult: String

    public init(hrtfSet: String, pretestResult: String) {
        self.hrtfSet = hrtfSet
        self.pretestResult = pretestResult
    }
}

public struct AbxEquipmentRecord: Sendable, Equatable {
    public var headphones: String
    public var outputPath: String
    public var monitorGainDB: Float?

    public init(headphones: String, outputPath: String, monitorGainDB: Float? = nil) {
        self.headphones = headphones
        self.outputPath = outputPath
        self.monitorGainDB = monitorGainDB
    }
}

public struct AbxSignOff: Sendable, Equatable {
    public var listenerSigned: String
    public var dateISO: String

    public init(listenerSigned: String, dateISO: String) {
        self.listenerSigned = listenerSigned
        self.dateISO = dateISO
    }
}

public enum AbxChoice: String, Sendable {
    case a = "A"
    case b = "B"
}

public enum AbxPresentationToken: String, Sendable {
    case a = "A"
    case b = "B"
    case x = "X"
}

/// A deterministic trial plan. The host presents each token in
/// `presentedOrder`; for `.x`, it supplies stimulus `xAssignment`.
public struct AbxTrialPlan: Sendable {
    public let trialIndex: UInt32
    public let presentedOrder: [AbxPresentationToken]
    public let xAssignment: AbxChoice

    fileprivate var recordOrder: [String] {
        presentedOrder.map { token in
            token == .x ? "X=\(xAssignment.rawValue)" : token.rawValue
        }
    }
}

public enum AbxSessionError: Error, Sendable, CustomStringConvertible {
    case tooFewTrials(minimum: Int, actual: Int)
    case tooManyTrials
    case invalidTrialIndex(UInt32)
    case duplicateResponse(UInt32)
    case incompleteSession(completed: Int, required: Int)
    case missingField(String)

    public var description: String {
        switch self {
        case let .tooFewTrials(minimum, actual):
            "ABX session requires at least \(minimum) trials, found \(actual)"
        case .tooManyTrials:
            "ABX session has more trials than the UInt32 schema index permits"
        case let .invalidTrialIndex(index):
            "ABX trial index \(index) does not exist"
        case let .duplicateResponse(index):
            "ABX trial \(index) already has a response"
        case let .incompleteSession(completed, required):
            "ABX session has \(completed) completed responses; \(required) are required"
        case let .missingField(field):
            "ABX session is missing \(field)"
        }
    }
}

/// Seeded ABX trial sequencing and record emission for an externalization test.
///
/// This scaffold does not load or play audio. The host app owns stimuli and
/// playback: present `A`, `B`, and `X` in each ``AbxTrialPlan/presentedOrder``,
/// using ``AbxTrialPlan/xAssignment`` to choose the hidden X stimulus, then call
/// ``recordResponse(_:forTrial:)``. X assignments are balanced before a seeded
/// shuffle, and each trial's A/B/X presentation order is independently shuffled
/// by the same SplitMix64 stream.
///
/// ``emitJSON()`` emits whitespace-free UTF-8 matching the Rust
/// `fightbox.abx.v1` fixed field order and token convention. The seed is part of
/// the record. One instance is one (listener, HRTF set, device, head-tracking)
/// tuple; changing any member requires a new session.
public final class AbxSession {
    public let sessionID: String
    public let listener: AbxListenerIdentity
    public let hrtf: AbxHrtfRecord
    public let equipment: AbxEquipmentRecord
    public let device: String
    public let headTrackingEnabled: Bool
    public let seed: UInt64
    public let dateISO: String
    public var signOff: AbxSignOff
    public let trials: [AbxTrialPlan]
    public var claims: [String] = []
    public private(set) var nonClaims: [String] = [FIGHTBOX_ABX_REQUIRES_HUMAN]

    private var responses: [AbxChoice?]

    public init(
        sessionID: String,
        listener: AbxListenerIdentity,
        hrtf: AbxHrtfRecord,
        equipment: AbxEquipmentRecord,
        device: String,
        headTrackingEnabled: Bool,
        seed: UInt64,
        trialCount: Int = FIGHTBOX_ABX_MIN_TRIALS,
        dateISO: String,
        signOff: AbxSignOff
    ) throws {
        guard trialCount >= FIGHTBOX_ABX_MIN_TRIALS else {
            throw AbxSessionError.tooFewTrials(
                minimum: FIGHTBOX_ABX_MIN_TRIALS,
                actual: trialCount
            )
        }
        guard trialCount <= UInt32.max else {
            throw AbxSessionError.tooManyTrials
        }
        self.sessionID = sessionID
        self.listener = listener
        self.hrtf = hrtf
        self.equipment = equipment
        self.device = device
        self.headTrackingEnabled = headTrackingEnabled
        self.seed = seed
        self.dateISO = dateISO
        self.signOff = signOff

        var generator = SplitMix64(seed: seed)
        var assignments = (0 ..< trialCount).map { index in
            index.isMultiple(of: 2) ? AbxChoice.a : AbxChoice.b
        }
        generator.shuffle(&assignments)
        trials = assignments.enumerated().map { offset, assignment in
            var order = [
                AbxPresentationToken.a,
                AbxPresentationToken.b,
                AbxPresentationToken.x,
            ]
            generator.shuffle(&order)
            return AbxTrialPlan(
                trialIndex: UInt32(offset + 1),
                presentedOrder: order,
                xAssignment: assignment
            )
        }
        responses = Array(repeating: nil, count: trialCount)
    }

    public var nextUnansweredTrial: AbxTrialPlan? {
        guard let offset = responses.firstIndex(where: { $0 == nil }) else {
            return nil
        }
        return trials[offset]
    }

    public func recordResponse(_ response: AbxChoice, forTrial trialIndex: UInt32) throws {
        guard trialIndex > 0, Int(trialIndex) <= responses.count else {
            throw AbxSessionError.invalidTrialIndex(trialIndex)
        }
        let offset = Int(trialIndex - 1)
        guard responses[offset] == nil else {
            throw AbxSessionError.duplicateResponse(trialIndex)
        }
        responses[offset] = response
    }

    public func appendNonClaim(_ statement: String) {
        nonClaims.append(statement)
    }

    public var completedTrialCount: Int {
        responses.compactMap { $0 }.count
    }

    public var correctCount: Int {
        zip(trials, responses).reduce(into: 0) { count, pair in
            if pair.1 == pair.0.xAssignment {
                count += 1
            }
        }
    }

    public var exactBinomialOneSidedPValue: Double {
        Self.binomialTail(correct: correctCount, trials: trials.count)
    }

    /// Emit a complete `fightbox.abx.v1` record. Incomplete responses or blank
    /// identity/equipment/sign-off fields are rejected.
    public func emitJSON() throws -> String {
        try validateForEmission()

        var object = JsonBytes()
        object.string("schema_version", FIGHTBOX_ABX_SCHEMA_VERSION)
        object.string("session_id", sessionID)
        object.raw(
            "listener",
            JsonBytes.object {
                $0.string("listener_id", listener.listenerID)
                $0.string("notes", listener.notes)
            }
        )
        object.raw(
            "hrtf",
            JsonBytes.object {
                $0.string("hrtf_set", hrtf.hrtfSet)
                $0.string("pretest_result", hrtf.pretestResult)
            }
        )
        object.raw(
            "equipment",
            JsonBytes.object {
                $0.string("headphones", equipment.headphones)
                $0.string("output_path", equipment.outputPath)
                $0.optionalFloat("monitor_gain_db", equipment.monitorGainDB)
            }
        )
        object.string("device", device)
        object.boolean("head_tracking_enabled", headTrackingEnabled)
        object.raw("seed", String(seed))

        let trialJSON = zip(trials, responses).map { trial, response in
            JsonBytes.object {
                $0.raw("trial_index", String(trial.trialIndex))
                $0.raw(
                    "presented_order",
                    JsonBytes.stringArray(trial.recordOrder)
                )
                $0.string("response", response!.rawValue)
                $0.boolean("correct", response == trial.xAssignment)
            }
        }.joined(separator: ",")
        object.raw("trials", "[\(trialJSON)]")
        object.raw("correct_count", String(correctCount))
        object.raw("trial_count", String(trials.count))
        object.raw(
            "exact_binomial_one_sided_p_value",
            JsonBytes.double(exactBinomialOneSidedPValue)
        )
        object.string("date_iso", dateISO)
        object.raw(
            "sign_off",
            JsonBytes.object {
                $0.string("listener_signed", signOff.listenerSigned)
                $0.string("date_iso", signOff.dateISO)
            }
        )
        object.boolean("requires_human_completion", true)
        object.raw("claims", JsonBytes.stringArray(claims))
        object.raw("non_claims", JsonBytes.stringArray(nonClaims))
        return object.finish()
    }

    private func validateForEmission() throws {
        let fields = [
            ("session_id", sessionID),
            ("listener.listener_id", listener.listenerID),
            ("hrtf.hrtf_set", hrtf.hrtfSet),
            ("hrtf.pretest_result", hrtf.pretestResult),
            ("equipment.headphones", equipment.headphones),
            ("equipment.output_path", equipment.outputPath),
            ("device", device),
            ("date_iso", dateISO),
            ("sign_off.listener_signed", signOff.listenerSigned),
            ("sign_off.date_iso", signOff.dateISO),
        ]
        if let missing = fields.first(where: { $0.1.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }) {
            throw AbxSessionError.missingField(missing.0)
        }
        guard completedTrialCount == trials.count else {
            throw AbxSessionError.incompleteSession(
                completed: completedTrialCount,
                required: trials.count
            )
        }
    }

    private static func binomialTail(correct: Int, trials: Int) -> Double {
        guard correct > 0 else { return 1 }
        var combination = 1.0
        var numerator = 0.0
        for successes in 0 ... trials {
            if successes >= correct {
                numerator += combination
            }
            if successes < trials {
                combination *= Double(trials - successes) / Double(successes + 1)
            }
        }
        return numerator * pow(0.5, Double(trials))
    }
}

private struct SplitMix64 {
    private var state: UInt64

    init(seed: UInt64) {
        state = seed
    }

    mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var value = state
        value = (value ^ (value >> 30)) &* 0xBF58_476D_1CE4_E5B9
        value = (value ^ (value >> 27)) &* 0x94D0_49BB_1331_11EB
        return value ^ (value >> 31)
    }

    mutating func shuffle<Element>(_ values: inout [Element]) {
        guard values.count > 1 else { return }
        for upper in stride(from: values.count - 1, through: 1, by: -1) {
            let selected = Int(next() % UInt64(upper + 1))
            values.swapAt(upper, selected)
        }
    }
}

private struct JsonBytes {
    private var output = "{"
    private var needsComma = false

    mutating func raw(_ key: String, _ value: String) {
        if needsComma {
            output.append(",")
        }
        needsComma = true
        output.append(Self.string(key))
        output.append(":")
        output.append(value)
    }

    mutating func string(_ key: String, _ value: String) {
        raw(key, Self.string(value))
    }

    mutating func boolean(_ key: String, _ value: Bool) {
        raw(key, value ? "true" : "false")
    }

    mutating func optionalFloat(_ key: String, _ value: Float?) {
        raw(key, value.map(Self.float) ?? "null")
    }

    mutating func finish() -> String {
        output.append("}")
        return output
    }

    static func object(_ body: (inout JsonBytes) -> Void) -> String {
        var object = JsonBytes()
        body(&object)
        return object.finish()
    }

    static func stringArray(_ values: [String]) -> String {
        "[\(values.map(Self.string).joined(separator: ","))]"
    }

    static func string(_ value: String) -> String {
        var escaped = "\""
        for scalar in value.unicodeScalars {
            switch scalar.value {
            case 0x08:
                escaped.append("\\b")
            case 0x09:
                escaped.append("\\t")
            case 0x0A:
                escaped.append("\\n")
            case 0x0C:
                escaped.append("\\f")
            case 0x0D:
                escaped.append("\\r")
            case 0x22:
                escaped.append("\\\"")
            case 0x5C:
                escaped.append("\\\\")
            case 0x00 ... 0x1F:
                escaped.append(String(format: "\\u%04x", scalar.value))
            default:
                escaped.unicodeScalars.append(scalar)
            }
        }
        escaped.append("\"")
        return escaped
    }

    static func float(_ value: Float) -> String {
        guard value.isFinite else { return "null" }
        if value.rounded() == value, value >= Float(Int64.min), value <= Float(Int64.max) {
            return String(Int64(value))
        }
        return String(value)
    }

    static func double(_ value: Double) -> String {
        guard value.isFinite else { return "null" }
        if value.rounded() == value, value >= Double(Int64.min), value <= Double(Int64.max) {
            return String(Int64(value))
        }
        return String(value)
    }
}
