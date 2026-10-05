import CoreMotion
import Darwin
import Foundation

// stdout: 48-byte little-endian FHT1, state:u32, uptime:f64, quaternion w/x/y/z:f64.
private enum State: UInt32 {
    case waiting = 0, tracking = 1, denied = 2, noHeadphones = 3
    case notSupported = 4, authorizedWaiting = 5, error = 6
}

private let identity = CMQuaternion(x: 0, y: 0, z: 0, w: 1)

private func writeRecord(_ state: State, _ timestamp: Double, _ quaternion: CMQuaternion = identity) {
    var record = Data("FHT1".utf8)
    var rawState = state.rawValue.littleEndian
    withUnsafeBytes(of: &rawState) { record.append(contentsOf: $0) }
    for value in [timestamp, quaternion.w, quaternion.x, quaternion.y, quaternion.z] {
        var bits = value.bitPattern.littleEndian
        withUnsafeBytes(of: &bits) { record.append(contentsOf: $0) }
    }
    record.withUnsafeBytes { bytes in
        var offset = 0
        while offset < bytes.count {
            let count = Darwin.write(STDOUT_FILENO, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            if count < 0 {
                if errno == EINTR { continue }
                exit(0) // The Workbench closed its pipe.
            }
            offset += count
        }
    }
}

private func diagnostic(_ message: String) {
    FileHandle.standardError.write(Data((message + "\n").utf8))
}

@available(macOS 14.0, *)
private final class HeadTracker: NSObject, CMHeadphoneMotionManagerDelegate, @unchecked Sendable {
    private let manager = CMHeadphoneMotionManager()
    private let probe: Bool
    private let parentPID = getppid()
    private var timer: Timer?
    private var lastSampleAt: Double?
    private var updatesRequested = false
    private var lastError: String?
    private var lastStatusLine: String?
    private var guided = GuidedProbe(started: ProcessInfo.processInfo.systemUptime)
    private var snapshot: MotionSnapshot?
    private var speech: Process?

    init(probe: Bool) {
        self.probe = probe
        super.init()
    }

    func start() {
        manager.delegate = self
        manager.startConnectionStatusUpdates()
        requestMotionIfAvailable()
        publishStatus()
        timer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            guard let self else { return }
            if getppid() != self.parentPID {
                self.stop()
                exit(0)
            }
            self.requestMotionIfAvailable()
            self.publishStatus()
        }
    }

    private func requestMotionIfAvailable() {
        let authorization = CMHeadphoneMotionManager.authorizationStatus()
        if !updatesRequested && manager.isDeviceMotionAvailable && authorization != .denied && authorization != .restricted {
            updatesRequested = true
            // Requesting updates is what requests motion permission on first use.
            // Main queue serializes the delegate, samples and status timer.
            manager.startDeviceMotionUpdates(to: .main) { [weak self] motion, error in
                guard let self else { return }
                if let error {
                    self.lastError = error.localizedDescription
                    if self.probe { print("\nCoreMotion: \(error.localizedDescription)") }
                    else { diagnostic("CoreMotion: \(error.localizedDescription)") }
                    self.publishStatus()
                    return
                }
                guard let motion else { return }
                let now = ProcessInfo.processInfo.systemUptime
                guard motion.timestamp.isFinite && motion.timestamp >= 0 && motion.timestamp <= now
                    && now - motion.timestamp <= 0.5
                    && self.lastSampleAt.map({ motion.timestamp > $0 }) ?? true else { return }
                let raw = motion.attitude.quaternion
                let snapshot = MotionSnapshot(timestamp: motion.timestamp,
                    rawQuaternion: [raw.w, raw.x, raw.y, raw.z], rawYaw: motion.attitude.yaw,
                    rawPitch: motion.attitude.pitch, rawRoll: motion.attitude.roll)
                guard snapshot.isFresh(at: now) else { return }
                self.lastError = nil
                self.lastSampleAt = motion.timestamp
                self.snapshot = snapshot
                if !self.probe {
                    writeRecord(.tracking, motion.timestamp, motion.attitude.quaternion)
                }
            }
        }
    }

    private func authorizationLabel() -> String {
        switch CMHeadphoneMotionManager.authorizationStatus() {
        case .authorized: return "authorized"
        case .denied: return "denied"
        case .restricted: return "denied (restricted)"
        case .notDetermined: return "waiting for motion permission"
        @unknown default: return "unknown authorization"
        }
    }

    private func state() -> State {
        let authorization = CMHeadphoneMotionManager.authorizationStatus()
        if authorization == .denied || authorization == .restricted { return .denied }
        if lastError != nil { return .error }
        if !manager.isDeviceMotionAvailable { return .noHeadphones }
        if let lastSampleAt, ProcessInfo.processInfo.systemUptime - lastSampleAt <= 0.5 {
            return .tracking
        }
        return authorization == .authorized ? .authorizedWaiting : .waiting
    }

    private func publishStatus() {
        let current = state()
        let availability = manager.isDeviceMotionAvailable ? "motion available" : "no headphones"
        let line = "\(authorizationLabel()); isDeviceMotionAvailable=\(manager.isDeviceMotionAvailable) (\(availability))"
        if line != lastStatusLine {
            if probe { print("\n\(line)") } else { diagnostic(line) }
            lastStatusLine = line
        }
        if probe {
            let now = ProcessInfo.processInfo.systemUptime
            let failure: String?
            switch current {
            case .denied: failure = "\(authorizationLabel()); allow Fightbox Head Tracker in System Settings → Privacy & Security → Motion & Fitness"
            case .error: failure = lastError ?? "CoreMotion error"
            case .notSupported: failure = "not supported on this Mac"
            default: failure = nil
            }
            let lines = guided.tick(now: now,
                authorized: CMHeadphoneMotionManager.authorizationStatus() == .authorized,
                available: manager.isDeviceMotionAvailable, failure: failure, snapshot: snapshot)
            for line in lines { print("\n\(line)") }
            if let cue = guided.takeCue() { speak(cue) }
            if guided.phase == .finished {
                stop()
                exit(guided.succeeded ? 0 : 1)
            }
            let text = "\r" + guided.liveLine(now: now, snapshot: snapshot) + "\u{1B}[K"
            FileHandle.standardOutput.write(Data(text.utf8))
        } else if current != .tracking {
            // A heartbeat never promotes an old pose to a fresh motion record.
            writeRecord(current, ProcessInfo.processInfo.systemUptime)
        }
    }

    private func speak(_ text: String) {
        // The user is turned away from the screen; speak through the current output.
        speech?.terminate()
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/say")
        process.arguments = [text]
        try? process.run()
        speech = process
    }

    private func stop() {
        timer?.invalidate()
        manager.stopDeviceMotionUpdates()
        manager.stopConnectionStatusUpdates()
    }

    func headphoneMotionManagerDidConnect(_ manager: CMHeadphoneMotionManager) {
        lastError = nil
        requestMotionIfAvailable()
        publishStatus()
    }

    func headphoneMotionManagerDidDisconnect(_ manager: CMHeadphoneMotionManager) {
        lastSampleAt = nil
        snapshot = nil
        lastError = nil
        manager.stopDeviceMotionUpdates()
        updatesRequested = false
        publishStatus()
    }
}

signal(SIGPIPE, SIG_IGN)
if CommandLine.arguments.contains("--self-test") {
    // This branch runs only pure mapping/guide code, before manager initialization.
    do {
        for line in try runSafeSelfTest() { print(line) }
        exit(0)
    } catch {
        diagnostic("Safe self-test failed: \(error)")
        exit(1)
    }
}
if CommandLine.arguments.contains("--inspect") {
    // This route must stay free of CoreMotion API calls and permission requests.
    print("HeadTracker: FHT1 / 48-byte little-endian records; macOS 14+; ad-hoc local app")
    print("NSMotionUsageDescription=\(Bundle.main.object(forInfoDictionaryKey: "NSMotionUsageDescription") ?? "missing")")
    print("LSUIElement=\(Bundle.main.object(forInfoDictionaryKey: "LSUIElement") ?? "missing")")
    exit(0)
}
// TCC holds the launching app (Terminal, or the Terminal-launched Workbench)
// responsible for a child's motion request, and Terminal declares no motion
// purpose string. Respawn once, disclaimed, so macOS asks for this bundle.
@_silgen_name("responsibility_spawnattrs_setdisclaim")
private func setDisclaim(_ attributes: UnsafeMutablePointer<posix_spawnattr_t?>, _ disclaim: Int32) -> Int32

private func runDisclaimed() -> Int32? {
    let key = "FIGHTBOX_HEADTRACKER_DISCLAIMED"
    var environment = ProcessInfo.processInfo.environment
    guard environment[key] == nil, let path = Bundle.main.executablePath else { return nil }
    var attributes: posix_spawnattr_t?
    guard posix_spawnattr_init(&attributes) == 0 else { return nil }
    defer { posix_spawnattr_destroy(&attributes) }
    guard setDisclaim(&attributes, 1) == 0 else { return nil }
    environment[key] = "1"
    let argv = CommandLine.arguments.map { strdup($0) } + [nil]
    let envp = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
    defer { (argv + envp).forEach { free($0) } }
    var child: pid_t = 0
    guard posix_spawn(&child, path, nil, &attributes, argv, envp) == 0 else { return nil }
    var status: Int32 = 0
    while waitpid(child, &status, 0) < 0 && errno == EINTR {}
    let signal = status & 0x7f
    return signal == 0 ? (status >> 8) & 0xff : 128 + signal
}

if let status = runDisclaimed() { exit(status) }
let probe = CommandLine.arguments.contains("--probe")
if probe { print("Waiting up to 20 seconds for authorized, fresh headphone motion.") }
if #available(macOS 14.0, *) {
    let tracker = HeadTracker(probe: probe)
    tracker.start()
    withExtendedLifetime(tracker) { RunLoop.main.run() }
} else {
    if probe { print("Head tracking failed: not supported; macOS 14 or newer is required.") }
    else { writeRecord(.notSupported, ProcessInfo.processInfo.systemUptime) }
    exit(1)
}
