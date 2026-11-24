import AppKit
import Foundation
import Observation

@Observable
@MainActor
final class Store {
    enum Connection: Equatable { case connecting, connected, unavailable(String) }

    var connection: Connection = .connecting
    var version = ""
    var pid = 0
    var account: AccountInfo?
    var loggedIn = false
    var devices: [DeviceInfo] = []
    var apps: [AppRecord] = []
    var jobs: [JobInfo] = []
    var prompts: [Prompt] = []
    var config: DaemonConfig?
    var nextCheckAt: Int?
    var logs: [LogLine] = []
    var banner: Banner?

    // Setup / onboarding
    var setup: SetupReport?
    var showOnboarding = false
    /// Why the background service couldn't be started (from `daemon install`).
    var serviceError: ServiceError?
    /// Sidebar navigation requests from views (e.g. "Sign in" in a checklist).
    var navigate: String?

    struct ServiceError: Equatable {
        var code: String
        var message: String
    }

    // Account data loaded on demand.
    var appIds: AppIdList?
    var certs: [CertSummary]?
    var teams: TeamsResult?

    struct Banner: Identifiable, Equatable {
        let id = UUID()
        var title: String
        var message: String
        var isError: Bool
    }

    private let client = DaemonClient()
    private var reconnectTask: Task<Void, Never>?
    private var triedInstallingAgent = false

    init() {
        client.onEvent = { [weak self] ev in Task { @MainActor in self?.apply(event: ev) } }
        client.onDisconnect = { [weak self] in Task { @MainActor in self?.disconnected() } }
        start()
    }

    // MARK: Connection

    func start() {
        reconnectTask?.cancel()
        reconnectTask = Task { [weak self] in
            var attempt = 0
            while !Task.isCancelled {
                guard let self else { return }
                if await self.tryConnect() { return }
                attempt += 1
                if attempt == 1 { self.bootstrapDaemon() }
                if attempt > 4 {
                    self.connection = .unavailable("The Loadside background service isn't running.")
                }
                try? await Task.sleep(for: .seconds(min(Double(attempt), 5)))
            }
        }
    }

    private func tryConnect() async -> Bool {
        do {
            try await client.connect()
        } catch {
            return false
        }
        do {
            let snap = try await client.call("subscribe", .object(["interactive": .bool(true)]), as: Snapshot.self)
            apply(snapshot: snap)
            connection = .connected
            serviceError = nil
            Task { await loadSetup() }
            if let lines = try? await client.call("logs", .object(["limit": .number(400)]), as: [LogLine].self) {
                logs = lines
            }
            return true
        } catch {
            client.disconnect()
            return false
        }
    }

    private func disconnected() {
        connection = .connecting
        start()
    }

    // MARK: File hand-off

    /// Loadside's data folder (same rules as the daemon's `data_dir`).
    nonisolated static var dataDir: URL {
        if let p = ProcessInfo.processInfo.environment["LOADSIDE_HOME"] { return URL(fileURLWithPath: p) }
        return FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Loadside")
    }

    /// Copy a file the user picked into Loadside's own folder, so the
    /// background service never needs access to Downloads/Desktop/Documents
    /// (macOS would otherwise ask the user to allow "loadsided").
    nonisolated static func stage(_ src: URL) async throws -> URL {
        try await Task.detached(priority: .userInitiated) {
            let root = dataDir.standardizedFileURL.path
            if src.standardizedFileURL.path.hasPrefix(root + "/") { return src }
            let dir = dataDir.appendingPathComponent("Incoming").appendingPathComponent(UUID().uuidString)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let dst = dir.appendingPathComponent(src.lastPathComponent)
            try FileManager.default.copyItem(at: src, to: dst)
            return dst
        }.value
    }

    /// Remove a staged copy (only ever inside Incoming).
    /// Only the per-file folder `stage` created (Incoming/<uuid>/) is removed.
    nonisolated static func unstage(_ path: String) {
        let incoming = dataDir.appendingPathComponent("Incoming").standardizedFileURL.path
        let dir = URL(fileURLWithPath: path).standardizedFileURL.deletingLastPathComponent()
        guard dir.deletingLastPathComponent().path == incoming, UUID(uuidString: dir.lastPathComponent) != nil else { return }
        try? FileManager.default.removeItem(at: dir)
    }

    /// Where the daemon writes an exported IPA before the app moves it.
    nonisolated static func exportStaging(name: String) throws -> URL {
        let dir = dataDir.appendingPathComponent("Exports").appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent(name)
    }

    /// job id → where the user wanted the exported IPA.
    var exportDestinations: [String: URL] = [:]

    private func finishExport(_ job: JobInfo) {
        guard let dest = exportDestinations[job.id], let out = job.outputPath else { return }
        exportDestinations[job.id] = nil
        let src = URL(fileURLWithPath: out)
        do {
            if FileManager.default.fileExists(atPath: dest.path) { try FileManager.default.removeItem(at: dest) }
            try FileManager.default.moveItem(at: src, to: dest)
            try? FileManager.default.removeItem(at: src.deletingLastPathComponent())
            exportDestinations[job.id] = dest // remembered for "Show in Finder"
        } catch {
            banner = Banner(title: "Couldn't save the signed IPA", message: "It's at \(out). (\(error.localizedDescription))", isError: true)
        }
    }

    /// Paths of the helper binaries shipped inside Loadside.app.
    static var bundledCLI: URL? {
        let u = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/loadside-cli")
        return FileManager.default.isExecutableFile(atPath: u.path) ? u : nil
    }
    static var bundledDaemon: URL? {
        let u = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/loadsided")
        return FileManager.default.isExecutableFile(atPath: u.path) ? u : nil
    }

    /// Make sure the LaunchAgent points at the daemon inside this app bundle.
    /// Runs off the main thread and records a readable error if it fails.
    private var bootstrapping = false

    func bootstrapDaemon(force: Bool = false) {
        // A quarantined app runs from a random read-only copy; pointing the
        // LaunchAgent there would break as soon as that copy disappears.
        if Bundle.main.bundlePath.contains("/AppTranslocation/") {
            serviceError = ServiceError(code: "translocated", message: "Move Loadside to your Applications folder, then open it again.")
            return
        }
        if bootstrapping { return }
        guard let cli = Self.bundledCLI, let daemon = Self.bundledDaemon else {
            if Bundle.main.bundleURL.pathExtension == "app" {
                serviceError = ServiceError(code: "missing_binary", message: "Loadside.app is missing its background service. Reinstall the app.")
            }
            return
        }
        if triedInstallingAgent && !force { return }
        triedInstallingAgent = true
        bootstrapping = true
        Task.detached {
            let p = Process()
            p.executableURL = cli
            p.arguments = ["daemon", "install", "--bin", daemon.path]
            let errPipe = Pipe()
            p.standardError = errPipe
            p.standardOutput = Pipe()
            var err: ServiceError?
            do {
                try p.run()
                p.waitUntilExit()
                let text = String(data: errPipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
                if p.terminationStatus != 0 { err = Self.parseServiceError(text) }
            } catch {
                err = ServiceError(code: "launch_failed", message: error.localizedDescription)
            }
            await MainActor.run { [err] in
                self.bootstrapping = false
                if let err { self.serviceError = err }
            }
        }
    }

    nonisolated static func parseServiceError(_ text: String) -> ServiceError {
        var t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if t.hasPrefix("error: ") { t = String(t.dropFirst(7)) }
        var code = "bootstrap_failed"
        if t.hasPrefix("["), let end = t.firstIndex(of: "]") {
            code = String(t[t.index(after: t.startIndex)..<end])
            t = String(t[t.index(after: end)...]).trimmingCharacters(in: .whitespaces)
        }
        return ServiceError(code: code, message: t.isEmpty ? "The background service couldn't be started." : t)
    }

    func restartDaemon() async {
        _ = try? await client.call("shutdown")
        try? await Task.sleep(for: .milliseconds(600))
        bootstrapDaemon(force: true)
        start()
    }

    // MARK: State

    private func apply(snapshot s: Snapshot) {
        version = s.version
        pid = s.pid
        account = s.account
        loggedIn = s.loggedIn
        devices = s.devices
        apps = s.apps
        jobs = s.jobs
        prompts = s.prompts
        // Screenshot/QA aid: show a prompt sheet without a real sign-in.
        if let kind = ProcessInfo.processInfo.environment["LOADSIDE_DEMO_PROMPT"] {
            prompts.append(Prompt(id: "demo", kind: kind, payload: .object([:]), createdAt: 0))
        }
        config = s.config
        nextCheckAt = s.nextCheckAt
    }

    private func apply(event ev: JSON) {
        switch ev["kind"]?.string {
        case "job":
            guard let j = try? ev["job"]?.decode(JobInfo.self) else { return }
            if let i = jobs.firstIndex(where: { $0.id == j.id }) { jobs[i] = j } else { jobs.append(j) }
            if j.status == .succeeded, j.kind == .install || j.kind == .refresh {
                // Account quotas changed.
                appIds = nil
                if account?.isFree == true { Task { await loadAppIds() } }
            }
            if j.status == .succeeded, j.kind == .export { finishExport(j) }
        case "devices":
            if let d = try? ev["devices"]?.decode([DeviceInfo].self) {
                devices = d
                Task { await loadSetup() }
            }
        case "apps":
            if let a = try? ev["apps"]?.decode([AppRecord].self) { apps = a }
        case "account":
            account = try? ev["account"]?.decode(AccountInfo.self)
            loggedIn = ev["loggedIn"]?.bool ?? ev["logged_in"]?.bool ?? false
            if !loggedIn && account == nil { appIds = nil; certs = nil; teams = nil }
            Task { await loadSetup() }
        case "prompt":
            if let p = try? ev["prompt"]?.decode(Prompt.self), !prompts.contains(where: { $0.id == p.id }) {
                prompts.append(p)
                NSApp.requestUserAttention(.criticalRequest)
            }
        case "promptClosed", "prompt_closed":
            let id = ev["id"]?.string
            prompts.removeAll { $0.id == id }
        case "log":
            if let l = try? ev["line"]?.decode(LogLine.self) {
                logs.append(l)
                if logs.count > 1500 { logs.removeFirst(logs.count - 1500) }
            }
        case "notice":
            banner = Banner(title: ev["title"]?.string ?? "", message: ev["message"]?.string ?? "", isError: ev["level"]?.string == "error")
        default: break
        }
    }

    // MARK: Helpers

    var activeJobs: [JobInfo] { jobs.filter(\.isActive) }
    func job(_ id: String?) -> JobInfo? { jobs.first { $0.id == id } }

    func show(_ error: Error) {
        let f = (error as? Failure) ?? Failure.local(error.localizedDescription)
        banner = Banner(title: f.message, message: f.hint ?? "", isError: true)
    }

    @discardableResult
    func run<T>(_ op: () async throws -> T) async -> T? {
        do { return try await op() } catch { show(error); return nil }
    }

    // MARK: API

    func login(email: String, password: String, remember: Bool) async throws {
        _ = try await client.call("login", .object(["email": .string(email), "password": .string(password), "remember": .bool(remember)]))
        await loadAccountDetails()
    }

    func logout() async { await run { _ = try await client.call("logout") } }

    func loadAccountDetails() async {
        guard account != nil else { return }
        async let t = try? client.call("teams", as: TeamsResult.self)
        async let a = try? client.call("app_ids", as: AppIdList.self)
        async let c = try? client.call("certs", as: [CertSummary].self)
        let (tt, aa, cc) = await (t, a, c)
        if let tt { teams = tt }
        if let aa { appIds = aa }
        if let cc { certs = cc }
    }

    /// Free accounts: make sure the weekly App ID budget is loaded.
    func ensureQuota() async {
        if account?.isFree == true, appIds == nil, connection == .connected { await loadAppIds() }
    }

    /// Free accounts can keep 3 sideloaded apps per device.
    static let freeSlotsPerDevice = 3

    /// Apps Loadside tracks on the device with the most of them.
    var slotsUsed: (device: String, used: Int)? {
        let live = apps.filter { $0.onDevice != false }
        let byDevice = Dictionary(grouping: live, by: \.deviceName)
        guard let top = byDevice.max(by: { $0.value.count < $1.value.count }) else {
            return devices.first.map { ($0.name, 0) }
        }
        return (top.key, top.value.count)
    }

    func loadAppIds() async { appIds = await run { try await client.call("app_ids", as: AppIdList.self) } ?? appIds }
    func loadCerts() async { certs = await run { try await client.call("certs", as: [CertSummary].self) } ?? certs }

    func selectTeam(_ id: String) async {
        await run { _ = try await client.call("select_team", .object(["team_id": .string(id)])) }
        await loadAccountDetails()
    }

    func revokeCert(_ serial: String) async {
        await run { _ = try await client.call("revoke_cert", .object(["serial_number": .string(serial)])) }
        await loadCerts()
    }

    func deleteAppId(_ id: String) async {
        await run { _ = try await client.call("delete_app_id", .object(["app_id_id": .string(id)])) }
        await loadAppIds()
    }

    func inspect(path: String, options: InstallOptions) async throws -> InspectResult {
        try await client.call("inspect", .object(["path": .string(path), "options": try JSON.from(options)]), as: InspectResult.self)
    }

    func install(_ params: InstallParams) async throws -> String {
        try await client.call("install", try JSON.from(params), as: JobRef.self).jobId
    }

    func refresh(_ app: AppRecord) async {
        // A deleted app gets reinstalled; otherwise this is a normal refresh.
        let reinstall = app.onDevice == false
        await run { _ = try await client.call("refresh", .object(["app_id": .string(app.id), "reinstall": .bool(reinstall)])) }
    }

    func refreshAll() async { await run { _ = try await client.call("refresh_all") } }

    func setAutoRefresh(_ app: AppRecord, _ on: Bool) async {
        await run { _ = try await client.call("update_app", .object(["app_id": .string(app.id), "auto_refresh": .bool(on)])) }
    }

    func remove(_ app: AppRecord, uninstall: Bool) async {
        await run { _ = try await client.call("remove_app", .object(["app_id": .string(app.id), "uninstall": .bool(uninstall)])) }
    }

    func refreshDevices() async {
        if let d = await run({ try await client.call("devices", .object(["refresh": .bool(true)]), as: [DeviceInfo].self) }) { devices = d }
    }

    func cancel(_ job: JobInfo) async { await run { _ = try await client.call("cancel_job", .object(["job_id": .string(job.id)])) } }

    func clearJobs() async {
        await run { _ = try await client.call("clear_jobs") }
        jobs.removeAll { !$0.isActive }
    }

    func respond(_ prompt: Prompt, _ value: JSON) async {
        await run { _ = try await client.call("respond", .object(["prompt_id": .string(prompt.id), "value": value])) }
        prompts.removeAll { $0.id == prompt.id }
    }

    func setConfig(_ patch: [String: JSON]) async {
        if let c = await run({ try await client.call("set_config", .object(["config": .object(patch)]), as: DaemonConfig.self) }) { config = c }
    }

    func resetAnisette() async {
        if await run({ _ = try await client.call("reset_anisette") }) != nil {
            banner = Banner(title: "Machine identity reset", message: "Sign in again to provision a new one.", isError: false)
        }
    }

    func checkNow() async { await run { _ = try await client.call("check_now") } }

    // MARK: Setup

    @ObservationIgnored private var loadingSetup = false
    @ObservationIgnored private var setupForceQueued = false

    func loadSetup(force: Bool = false) async {
        guard connection == .connected else { return }
        if loadingSetup {
            // Don't drop an explicit "check again" behind a background poll.
            if force { setupForceQueued = true }
            return
        }
        loadingSetup = true
        var f = force
        repeat {
            setupForceQueued = false
            if let r = try? await client.call("setup_status", .object(["force": .bool(f)]), as: SetupReport.self) {
                setup = r
                if !r.onboardingDone && setupDismissed == false { showOnboarding = true }
            }
            f = true
        } while setupForceQueued
        loadingSetup = false
    }

    @ObservationIgnored private var setupDismissed = false

    func finishOnboarding() async {
        showOnboarding = false
        setupDismissed = true
        await setConfig(["onboarding_done": .bool(true)])
        await loadSetup()
    }

    func reopenOnboarding() { setupDismissed = false; showOnboarding = true }

    func revealDeveloperMode(_ udid: String) async {
        if await run({ _ = try await client.call("reveal_developer_mode", .object(["udid": .string(udid)])) }) != nil {
            banner = Banner(title: "Developer Mode switch added", message: "On the device: Settings → Privacy & Security → Developer Mode → On, then restart.", isError: false)
        }
    }

    func enableWifiSync(_ udid: String) async {
        if await run({ _ = try await client.call("enable_wifi_sync", .object(["udid": .string(udid)])) }) != nil {
            banner = Banner(title: "Wi-Fi sync on", message: "The device now shows up over Wi-Fi while it's on the same network as this Mac.", isError: false)
            await loadSetup()
        }
    }

    /// Run a checklist action. Returns false if the view should handle it (e.g. "login").
    @discardableResult
    func perform(_ a: SetupAction) async -> Bool {
        switch a.id {
        case "open_url":
            if let s = a.url, let u = URL(string: s) { NSWorkspace.shared.open(u) }
        case "open_finder":
            NSWorkspace.shared.open(URL(fileURLWithPath: NSHomeDirectory()))
        case "open_settings": navigate = "settings"
        case "login": navigate = "account"; return false
        case "refresh_devices": await refreshDevices(); await loadSetup(force: true)
        case "retry": await loadSetup(force: true)
        case "reveal_developer_mode":
            if let u = a.deviceUdid { await revealDeveloperMode(u) }
        case "enable_wifi_sync":
            if let u = a.deviceUdid { await enableWifiSync(u) }
        default: break
        }
        return true
    }
}

// MARK: - Formatting

enum Fmt {
    static let relative: RelativeDateTimeFormatter = {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .full
        return f
    }()

    static func rel(_ ts: Int?) -> String {
        guard let ts else { return "unknown" }
        return relative.localizedString(for: Date(timeIntervalSince1970: TimeInterval(ts)), relativeTo: .now)
    }

    static func date(_ ts: Int?) -> String {
        guard let ts else { return "—" }
        return Date(timeIntervalSince1970: TimeInterval(ts)).formatted(date: .abbreviated, time: .shortened)
    }

    static func bytes(_ n: Int) -> String { ByteCountFormatter.string(fromByteCount: Int64(n), countStyle: .file) }
}
