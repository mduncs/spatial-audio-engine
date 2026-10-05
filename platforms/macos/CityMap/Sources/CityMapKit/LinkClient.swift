import Foundation
import Network

/// Loopback TCP client for the Workbench link. Reconnects every second while
/// the Workbench is not up. Loopback only, so no local-network prompt.
public final class LinkClient: @unchecked Sendable {
    public enum Status: Equatable, Sendable {
        case connecting
        case connected
        case waiting(String)
    }

    /// Both callbacks run on the main queue.
    public var onMessage: (Inbound) -> Void = { _ in }
    public var onStatus: (Status) -> Void = { _ in }

    public let port: UInt16
    private let queue = DispatchQueue(label: "citymap.link")
    private var connection: NWConnection?
    private var buffer = Data()
    private var stopped = false
    private static let maxLineBytes = 32 << 20

    public init(port: UInt16) {
        self.port = port
    }

    public func start() {
        queue.async { self.connect() }
    }

    public func stop() {
        queue.async {
            self.stopped = true
            self.connection?.cancel()
            self.connection = nil
        }
    }

    public func send(_ command: LinkCommand) {
        let data = Data((command.line() + "\n").utf8)
        queue.async {
            self.connection?.send(content: data, completion: .contentProcessed { _ in })
        }
    }

    private func report(_ status: Status) {
        DispatchQueue.main.async { self.onStatus(status) }
    }

    private func connect() {
        guard !stopped, let port = NWEndpoint.Port(rawValue: port) else { return }
        buffer.removeAll()
        let connection = NWConnection(host: "127.0.0.1", port: port, using: .tcp)
        self.connection = connection
        connection.stateUpdateHandler = { [weak self, weak connection] state in
            guard let self, let connection, connection === self.connection else { return }
            switch state {
            case .ready:
                self.report(.connected)
                self.receive(on: connection)
            case let .waiting(error), let .failed(error):
                self.retry(connection, because: "Waiting for the Workbench (\(error.localizedDescription))")
            case .cancelled:
                break
            default:
                self.report(.connecting)
            }
        }
        connection.start(queue: queue)
    }

    private func retry(_ connection: NWConnection, because reason: String) {
        guard connection === self.connection else { return }
        connection.cancel()
        self.connection = nil
        report(.waiting(reason))
        queue.asyncAfter(deadline: .now() + 1) { self.connect() }
    }

    private func receive(on connection: NWConnection) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 1 << 16) { [weak self] data, _, complete, error in
            guard let self, connection === self.connection else { return }
            if let data { self.buffer.append(data) }
            while let newline = self.buffer.firstIndex(of: 0x0A) {
                let line = self.buffer[self.buffer.startIndex..<newline]
                self.buffer.removeSubrange(self.buffer.startIndex...newline)
                guard !line.isEmpty, let message = try? Inbound.decode(Data(line)) else { continue }
                DispatchQueue.main.async { self.onMessage(message) }
            }
            if self.buffer.count > Self.maxLineBytes {
                self.retry(connection, because: "The Workbench sent an oversized line")
            } else if complete || error != nil {
                self.retry(connection, because: "The Workbench closed the link")
            } else {
                self.receive(on: connection)
            }
        }
    }
}
