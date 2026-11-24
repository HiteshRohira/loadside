import Foundation

/// Newline-delimited JSON over the daemon's Unix socket.
///
/// All state lives on one serial queue. Reads use a dispatch source whose
/// cancel handler closes the descriptor, so a read can never race a close.
/// Each connection has a generation number; callbacks from an older
/// connection are ignored.
final class DaemonClient: @unchecked Sendable {
    private let queue = DispatchQueue(label: "dev.loadside.client")
    private var fd: Int32 = -1
    private var source: DispatchSourceRead?
    private var generation = 0
    private var buffer = Data()
    private var nextId: UInt64 = 1
    private var pending: [UInt64: CheckedContinuation<JSON, Error>] = [:]

    /// Calls that may wait for a sign-in (and its 2FA code) take longer.
    /// Calls that can wait on Apple, a device or a sign-in in progress
    /// (the daemon has its own, shorter limits for each).
    static let longCalls: Set<String> = ["login", "logout", "teams", "select_team", "app_ids", "delete_app_id", "certs", "revoke_cert", "remove_app", "reveal_developer_mode", "enable_wifi_sync"]
    private static let maxLine = 64 * 1024 * 1024

    var onEvent: (@Sendable (JSON) -> Void)?
    var onDisconnect: (@Sendable () -> Void)?

    static var socketPath: String {
        if let p = ProcessInfo.processInfo.environment["LOADSIDE_SOCKET"] { return p }
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return support.appendingPathComponent("Loadside/loadsided.sock").path
    }

    /// Connect off the main thread.
    func connect(path: String = DaemonClient.socketPath) async throws {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            queue.async {
                do {
                    try self.connectLocked(path: path)
                    cont.resume()
                } catch {
                    cont.resume(throwing: error)
                }
            }
        }
    }

    private func connectLocked(path: String) throws {
        teardown(notify: false)
        let s = socket(AF_UNIX, SOCK_STREAM, 0)
        guard s >= 0 else { throw Failure.local("socket() failed") }
        var on: Int32 = 1
        setsockopt(s, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8CString)
        let cap = MemoryLayout.size(ofValue: addr.sun_path)
        guard bytes.count <= cap else { close(s); throw Failure.local("socket path too long") }
        withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
            ptr.withMemoryRebound(to: CChar.self, capacity: cap) { dst in
                for (i, b) in bytes.enumerated() { dst[i] = b }
            }
        }
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(s, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard r == 0 else { close(s); throw Failure.local("loadsided isn't running") }

        generation += 1
        let gen = generation
        fd = s
        buffer = Data()
        let src = DispatchSource.makeReadSource(fileDescriptor: s, queue: queue)
        src.setEventHandler { [weak self] in self?.readAvailable(gen: gen) }
        src.setCancelHandler { close(s) }
        source = src
        src.resume()
    }

    private func readAvailable(gen: Int) {
        guard gen == generation, fd >= 0 else { return }
        var chunk = [UInt8](repeating: 0, count: 65536)
        let n = chunk.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress, $0.count) }
        if n < 0 && (errno == EINTR || errno == EAGAIN) { return }
        if n <= 0 { teardown(notify: true); return }
        buffer.append(contentsOf: chunk[0..<n])
        if buffer.count > Self.maxLine && buffer.firstIndex(of: 0x0A) == nil {
            teardown(notify: true)
            return
        }
        while let nl = buffer.firstIndex(of: 0x0A) {
            let line = buffer[buffer.startIndex..<nl]
            buffer.removeSubrange(buffer.startIndex...nl)
            guard !line.isEmpty, let msg = try? Coders.decoder.decode(JSON.self, from: Data(line)) else { continue }
            switch msg["type"]?.string {
            case "response":
                let id = UInt64(msg["id"]?.int ?? 0)
                guard let cont = pending.removeValue(forKey: id) else { continue }
                if let err = msg["error"], err != .null {
                    cont.resume(throwing: (try? err.decode(Failure.self)) ?? Failure.local("loadsided returned an error Loadside couldn't read."))
                } else {
                    cont.resume(returning: msg["result"] ?? .null)
                }
            case "event":
                if let ev = msg["event"] { onEvent?(ev) }
            default: break
            }
        }
    }

    /// Close the connection. `notify` = it dropped on its own (reconnect).
    private func teardown(notify: Bool) {
        guard fd >= 0 else { return }
        source?.cancel() // closes fd in the cancel handler
        source = nil
        fd = -1
        generation += 1
        let waiting = pending
        pending = [:]
        for (_, c) in waiting { c.resume(throwing: Failure(code: "daemon_gone", message: "Lost connection to loadsided.", hint: nil, detail: nil)) }
        if notify { onDisconnect?() }
    }

    /// Intentional disconnect: doesn't trigger `onDisconnect`.
    func disconnect() { queue.async { self.teardown(notify: false) } }

    func call(_ method: String, _ params: JSON = .object([:]), timeout: TimeInterval? = nil) async throws -> JSON {
        let limit = timeout ?? (Self.longCalls.contains(method) ? 900 : 90)
        return try await withCheckedThrowingContinuation { (cont: CheckedContinuation<JSON, Error>) in
            queue.async {
                guard self.fd >= 0 else {
                    cont.resume(throwing: Failure(code: "daemon_gone", message: "Not connected to loadsided.", hint: nil, detail: nil))
                    return
                }
                let req: JSON = .object(["id": .number(Double(self.nextId)), "method": .string(method), "params": params])
                guard var data = try? JSONEncoder().encode(req) else {
                    cont.resume(throwing: Failure.local("Couldn't encode the request."))
                    return
                }
                let id = self.nextId
                self.nextId += 1
                self.pending[id] = cont
                data.append(0x0A)
                let fd = self.fd
                let ok = data.withUnsafeBytes { raw -> Bool in
                    var off = 0
                    while off < raw.count {
                        let n = write(fd, raw.baseAddress!.advanced(by: off), raw.count - off)
                        if n < 0 && errno == EINTR { continue }
                        if n <= 0 { return false }
                        off += n
                    }
                    return true
                }
                if !ok {
                    self.teardown(notify: true)
                    return
                }
                self.queue.asyncAfter(deadline: .now() + limit) {
                    if let c = self.pending.removeValue(forKey: id) {
                        c.resume(throwing: Failure(code: "timeout", message: "loadsided didn't answer in time.", hint: "Check Activity → Logs, or restart the service in Settings.", detail: nil))
                    }
                }
            }
        }
    }

    func call<T: Decodable>(_ method: String, _ params: JSON = .object([:]), as: T.Type, timeout: TimeInterval? = nil) async throws -> T {
        try await call(method, params, timeout: timeout).decode(T.self)
    }
}
