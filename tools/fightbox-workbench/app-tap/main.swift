import AppKit
import CoreAudio
import Darwin
import Foundation

struct AudioApp: Encodable {
    let label: String
    var processes: [UInt32]
}

enum TapError: Error, CustomStringConvertible {
    case hardware(String, OSStatus)
    case message(String)

    var description: String {
        switch self {
        case let .hardware(action, code): return "\(action): Core Audio error \(code)"
        case let .message(message): return message
        }
    }

    var denied: Bool {
        guard case let .hardware(action, code) = self else { return false }
        return code == kAudioDevicePermissionsError ||
            ((action == "start system audio recording" || action == "create process tap") &&
             code == kAudioHardwareIllegalOperationError)
    }
}

func diagnostic(_ message: String) {
    FileHandle.standardError.write(Data("Fightbox City Audio: \(message)\n".utf8))
}

func checked(_ code: OSStatus, _ action: String) throws {
    if code != noErr { throw TapError.hardware(action, code) }
}

func address(_ selector: AudioObjectPropertySelector,
             _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain)
}

func scalar<T>(_ object: AudioObjectID, _ selector: AudioObjectPropertySelector, into value: inout T) throws {
    var property = address(selector)
    var size = UInt32(MemoryLayout<T>.size)
    try withUnsafeMutablePointer(to: &value) { pointer in
        try checked(AudioObjectGetPropertyData(object, &property, 0, nil, &size, pointer), "read audio property")
    }
    guard size == MemoryLayout<T>.size else { throw TapError.message("invalid audio property size") }
}

func processObjects() throws -> [AudioObjectID] {
    var property = address(kAudioHardwarePropertyProcessObjectList)
    var size: UInt32 = 0
    try checked(AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &property, 0, nil, &size),
                "list audio processes")
    guard size % UInt32(MemoryLayout<AudioObjectID>.size) == 0 else {
        throw TapError.message("invalid audio process list")
    }
    var objects = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    if size > 0 {
        try objects.withUnsafeMutableBytes { bytes in
            try checked(AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &property,
                                                    0, nil, &size, bytes.baseAddress!), "read audio processes")
        }
    }
    return Array(objects.prefix(Int(size) / MemoryLayout<AudioObjectID>.size))
}

func translatedProcess(_ pid: pid_t) -> AudioObjectID? {
    var property = address(kAudioHardwarePropertyTranslatePIDToProcessObject)
    var pid = pid
    var object: AudioObjectID = 0
    var size = UInt32(MemoryLayout<AudioObjectID>.size)
    let code = withUnsafePointer(to: &pid) { pointer in
        AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &property,
                                   UInt32(MemoryLayout<pid_t>.size), pointer, &size, &object)
    }
    return code == noErr && object != 0 ? object : nil
}

func processPID(_ object: AudioObjectID) -> pid_t? {
    var pid: pid_t = 0
    try? scalar(object, kAudioProcessPropertyPID, into: &pid)
    return pid > 0 ? pid : nil
}

func processBundle(_ object: AudioObjectID) -> String? {
    var property = address(kAudioProcessPropertyBundleID)
    var value: Unmanaged<CFString>?
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    let code = AudioObjectGetPropertyData(object, &property, 0, nil, &size, &value)
    guard code == noErr, let value else { return nil }
    return value.takeRetainedValue() as String
}

func processPath(_ pid: pid_t) -> String? {
    var path = [CChar](repeating: 0, count: 4096)
    let length = path.withUnsafeMutableBufferPointer { buffer in
        fat_process_path(pid, buffer.baseAddress, UInt32(buffer.count))
    }
    guard length > 0 else { return nil }
    return path.withUnsafeBufferPointer { String(cString: $0.baseAddress!) }
}

func outerApp(_ path: String) -> String? {
    guard let range = path.range(of: ".app/") else { return nil }
    return String(path[..<range.lowerBound]) + ".app"
}

func knownApp(_ bundle: String) -> String? {
    if bundle == "com.apple.Music" { return "Music" }
    if bundle.hasPrefix("com.spotify.") { return "Spotify" }
    if bundle.hasPrefix("com.google.Chrome") { return "Google Chrome" }
    if bundle.hasPrefix("com.apple.Safari") { return "Safari" }
    return nil
}

func ownedApp(_ pid: pid_t, _ bundle: String?) -> (key: String, label: String) {
    var candidate = pid
    for _ in 0..<24 {
        if let path = processPath(candidate), let app = outerApp(path) {
            let url = URL(fileURLWithPath: app)
            let label = Bundle(url: url)?.object(forInfoDictionaryKey: "CFBundleDisplayName") as? String
                ?? Bundle(url: url)?.object(forInfoDictionaryKey: "CFBundleName") as? String
                ?? url.deletingPathExtension().lastPathComponent
            return (app, label)
        }
        let parent = fat_parent_pid(candidate)
        if parent <= 1 || parent == candidate { break }
        candidate = parent
    }
    if let bundle, let label = knownApp(bundle) { return (label, label) }
    if let bundle, bundle.hasPrefix("com.apple.WebKit") {
        return ("\(bundle):\(pid)", "WebKit audio (app unknown)")
    }
    let app = NSRunningApplication(processIdentifier: pid)
    return (bundle ?? "pid:\(pid)", app?.localizedName ?? bundle ?? "App \(pid)")
}

func excluded(_ pid: pid_t, bundle: String?, excludePID: pid_t) -> Bool {
    if pid == getpid() || pid == excludePID { return true }
    if bundle == "dev.fightbox.AudioTap" || bundle?.hasPrefix("dev.fightbox.Workbench") == true { return true }
    let name = processPath(pid).map { URL(fileURLWithPath: $0).lastPathComponent.lowercased() } ?? ""
    return name == "fightbox-workbench" || name == "fightboxaudiotap"
}

func audioApps(excludePID: pid_t) throws -> [AudioApp] {
    var apps: [String: AudioApp] = [:]
    for object in try processObjects() {
        guard let pid = processPID(object) else { continue }
        let bundle = processBundle(object)
        if excluded(pid, bundle: bundle, excludePID: excludePID) { continue }
        var running: UInt32 = 0
        try? scalar(object, kAudioProcessPropertyIsRunningOutput, into: &running)
        if running == 0 { continue }
        let owner = ownedApp(pid, bundle)
        if apps[owner.key] == nil { apps[owner.key] = AudioApp(label: owner.label, processes: []) }
        apps[owner.key]?.processes.append(object)
    }
    return apps.values.map { AudioApp(label: $0.label, processes: $0.processes.sorted()) }
        .sorted { $0.label.localizedCaseInsensitiveCompare($1.label) == .orderedAscending }
}

func excludedObjects(excludePID: pid_t) throws -> Set<AudioObjectID> {
    var objects: Set<AudioObjectID> = []
    for object in try processObjects() {
        if let pid = processPID(object), excluded(pid, bundle: processBundle(object), excludePID: excludePID) {
            objects.insert(object)
        }
    }
    for pid in [getpid(), excludePID] where pid > 0 {
        if let object = translatedProcess(pid) { objects.insert(object) }
    }
    return objects
}

func header(kind: UInt32, value: UInt32, count: UInt32) -> Data {
    var bytes = Data("FAT1".utf8)
    for number in [kind, value, count] {
        var little = number.littleEndian
        withUnsafeBytes(of: &little) { bytes.append(contentsOf: $0) }
    }
    return bytes
}

@discardableResult
func emit(_ data: Data, final: Bool = false) -> Bool {
    data.withUnsafeBytes { bytes in
        let pointer = bytes.bindMemory(to: UInt8.self).baseAddress!
        if final { fat_write_final(pointer, UInt32(bytes.count)); return true }
        return fat_write_bytes(pointer, UInt32(bytes.count))
    }
}

func sameFormat(_ lhs: AudioStreamBasicDescription, _ rhs: AudioStreamBasicDescription) -> Bool {
    lhs.mSampleRate == rhs.mSampleRate && lhs.mFormatID == rhs.mFormatID &&
    lhs.mFormatFlags == rhs.mFormatFlags && lhs.mBytesPerPacket == rhs.mBytesPerPacket &&
    lhs.mFramesPerPacket == rhs.mFramesPerPacket && lhs.mBytesPerFrame == rhs.mBytesPerFrame &&
    lhs.mChannelsPerFrame == rhs.mChannelsPerFrame && lhs.mBitsPerChannel == rhs.mBitsPerChannel
}

func orderedTeardown(stop: () -> Bool, destroyIO: () -> Bool, destroyAggregate: () -> Bool,
                     destroyTap: () -> Bool, freeContext: () -> Void) {
    _ = stop()
    let removedIO = destroyIO()
    let removedDevice = destroyAggregate()
    _ = destroyTap()
    if removedIO || removedDevice { freeContext() }
}

@available(macOS 14.2, *)
final class Capture {
    private var tap: AudioObjectID = 0
    private var aggregate: AudioObjectID = 0
    private var io: AudioDeviceIOProcID?
    private var ring: OpaquePointer?
    private var started = false
    private var format = AudioStreamBasicDescription()
    private var rate: UInt32 = 0
    private let selected: String
    private let excludePID: pid_t

    init(selected: String, excludePID: pid_t) {
        self.selected = selected
        self.excludePID = excludePID
    }

    func open() throws {
        let exclusions = try excludedObjects(excludePID: excludePID)
        // A global tap must identify the renderer before it starts, even if it has no output yet.
        guard let renderer = translatedProcess(excludePID), renderer != 0 else {
            throw TapError.message("workbench audio process is not ready; press Play again after output starts")
        }
        let description: CATapDescription
        if selected == "all" {
            description = CATapDescription(stereoGlobalTapButExcludeProcesses: Array(exclusions).sorted())
        } else {
            let parts = selected.split(separator: ",", omittingEmptySubsequences: false)
            let processes = parts.compactMap { UInt32($0) }.filter { $0 != 0 && !exclusions.contains($0) }
            guard processes.count == parts.count, !processes.isEmpty else {
                throw TapError.message("the selected app is unavailable")
            }
            description = CATapDescription(stereoMixdownOfProcesses: processes)
        }
        description.name = "Fightbox City Audio"
        description.uuid = UUID()
        description.isPrivate = true
        description.muteBehavior = .mutedWhenTapped
        try checked(AudioHardwareCreateProcessTap(description, &tap), "create process tap")
        try scalar(tap, kAudioTapPropertyFormat, into: &format)
        guard fat_format_valid(&format) else { throw TapError.message("unsupported tap format; expected native Float32 audio") }
        rate = UInt32(format.mSampleRate.rounded())
        guard let storage = fat_ring_create(&format) else { throw TapError.message("cannot allocate lock-free tap ring") }
        ring = storage
        let device: [String: Any] = [
            kAudioAggregateDeviceNameKey: "Fightbox City Audio",
            kAudioAggregateDeviceUIDKey: "dev.fightbox.AudioTap.\(UUID().uuidString)",
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceTapAutoStartKey: false,
            kAudioAggregateDeviceTapListKey: [[kAudioSubTapUIDKey: description.uuid.uuidString]]
        ]
        try checked(AudioHardwareCreateAggregateDevice(device as CFDictionary, &aggregate), "create private tap device")
        try checked(fat_create_io(aggregate, storage, &io),
                    "create tap callback")
        try checked(AudioDeviceStart(aggregate, io), "start system audio recording")
        started = true
    }

    func stream() throws {
        guard let ring else { throw TapError.message("tap ring missing") }
        guard emit(header(kind: 2, value: rate, count: 2)),
              emit(header(kind: 0, value: 1, count: 0)) else { return }
        var samples = [Float](repeating: 0, count: 4096 * 2)
        var nextCheck = ProcessInfo.processInfo.systemUptime + 0.25
        while !fat_control_stopped() {
            if fat_ring_invalid(ring) { throw TapError.message("tap channel format changed; stop and select the app again") }
            let now = ProcessInfo.processInfo.systemUptime
            if now >= nextCheck {
                var current = AudioStreamBasicDescription()
                try scalar(tap, kAudioTapPropertyFormat, into: &current)
                if !sameFormat(current, format) { throw TapError.message("tap sample format changed; stop and select the app again") }
                nextCheck = now + 0.25
            }
            let frames = samples.withUnsafeMutableBufferPointer { fat_ring_pop(ring, $0.baseAddress, 4096) }
            if frames == 0 { fat_control_wait(3); continue }
            var packet = header(kind: 1, value: rate, count: frames)
            packet.reserveCapacity(16 + Int(frames) * 8)
            for sample in samples.prefix(Int(frames) * 2) {
                var bits = sample.bitPattern.littleEndian
                withUnsafeBytes(of: &bits) { packet.append(contentsOf: $0) }
            }
            if !emit(packet) { break }
        }
    }

    func close() {
        orderedTeardown(stop: {
            guard self.started, let io = self.io else { return true }
            self.started = false
            return self.cleanup(AudioDeviceStop(self.aggregate, io), "stop tap callback")
        }, destroyIO: {
            guard let io = self.io else { return true }
            let removed = self.cleanup(AudioDeviceDestroyIOProcID(self.aggregate, io), "destroy tap callback")
            if removed { self.io = nil }
            return removed
        }, destroyAggregate: {
            guard self.aggregate != 0 else { return true }
            let removed = self.cleanup(AudioHardwareDestroyAggregateDevice(self.aggregate), "destroy tap device")
            if removed { self.aggregate = 0 }
            return removed
        }, destroyTap: {
            guard self.tap != 0 else { return true }
            let removed = self.cleanup(AudioHardwareDestroyProcessTap(self.tap), "destroy process tap")
            if removed { self.tap = 0 }
            return removed
        }, freeContext: {
            if let ring = self.ring {
                let dropped = fat_ring_dropped(ring)
                if dropped > 0 { diagnostic("pipe ring dropped \(dropped) frames") }
                fat_ring_destroy(ring)
                self.ring = nil
            }
        })
    }

    private func cleanup(_ code: OSStatus, _ action: String) -> Bool {
        if code != noErr { diagnostic("\(action): Core Audio error \(code)") }
        return code == noErr
    }
}

func selfTest() throws {
    let code = fat_ring_self_test()
    guard code == 0 else { throw TapError.message("ring/format self-test failed at \(code)") }
    let denied = header(kind: 0, value: 2, count: 0)
    guard denied == Data([70, 65, 84, 49, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]),
          header(kind: 2, value: 44100, count: 2).count == 16,
          TapError.hardware("start system audio recording", kAudioHardwareIllegalOperationError).denied,
          !TapError.message("format failure").denied else {
        throw TapError.message("protocol framing self-test failed")
    }
    var actions: [String] = []
    orderedTeardown(stop: { actions.append("stop"); return true },
                     destroyIO: { actions.append("io"); return true },
                     destroyAggregate: { actions.append("aggregate"); return true },
                     destroyTap: { actions.append("tap"); return true },
                     freeContext: { actions.append("free") })
    guard actions == ["stop", "io", "aggregate", "tap", "free"] else {
        throw TapError.message("teardown ordering self-test failed")
    }
    var freed = false
    orderedTeardown(stop: { false }, destroyIO: { false }, destroyAggregate: { false },
                     destroyTap: { true }, freeContext: { freed = true })
    guard !freed,
          outerApp("/Applications/Google Chrome.app/Contents/Frameworks/Helper.app/Contents/MacOS/Helper") ==
            "/Applications/Google Chrome.app" else { throw TapError.message("lifecycle/app grouping self-test failed") }
    print("4 tests passed: native formats/ring, protocol, teardown, app grouping (no hardware calls)")
}

func run() -> Int32 {
    var args = Array(CommandLine.arguments.dropFirst())
    if args == ["--self-test"] {
        do { try selfTest(); return 0 }
        catch { diagnostic(String(describing: error)); return 1 }
    }
    guard #available(macOS 14.2, *) else {
        diagnostic("app audio requires macOS 14.2 or later")
        if args.contains("--capture") { emit(header(kind: 0, value: 3, count: 0), final: true) }
        return 1
    }
    var excludePID: pid_t = 0
    if let index = args.firstIndex(of: "--exclude-pid"), index + 1 < args.count,
       let pid = pid_t(args[index + 1]), pid > 0 {
        excludePID = pid
        args.removeSubrange(index...index + 1)
    }
    if args == ["--list"] {
        do {
            let encoded = try JSONEncoder().encode(audioApps(excludePID: excludePID))
            FileHandle.standardOutput.write(encoded)
            FileHandle.standardOutput.write(Data([10]))
            return 0
        } catch { diagnostic(String(describing: error)); return 1 }
    }
    guard args.count == 2, args[0] == "--capture", excludePID > 0 else {
        diagnostic("usage: --list [--exclude-pid PID] | --capture all|PROCESS_IDS --exclude-pid PID | --self-test")
        return 1
    }
    fat_control_init(getppid())
    guard emit(header(kind: 0, value: 0, count: 0)) else { return 0 }
    let capture = Capture(selected: args[1], excludePID: excludePID)
    do {
        try capture.open()
        try capture.stream()
        capture.close()
        emit(header(kind: 0, value: 4, count: 0), final: true)
        return 0
    } catch {
        let denied = (error as? TapError)?.denied == true
        diagnostic(denied ? "System Audio Recording denied; allow Fightbox City Audio in System Settings → Privacy & Security" :
                    String(describing: error))
        capture.close()
        emit(header(kind: 0, value: denied ? 2 : 3, count: 0), final: true)
        return 1
    }
}

exit(run())
