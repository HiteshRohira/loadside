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
    var githubTokenSet = false
    var logs: [LogLine] = []
    var banner: Banner?

    // Setup / onboarding
    var setup: SetupReport?
    var showOnboarding = false
    /// Why the background service couldn't be started (from `daemon install`).
    var serviceError: ServiceError?
    /// Navigation requests from views and the menu bar ("install", "setup", "account"...).
    var navigate: String?
    /// A file to open in the install sheet (e.g. "Choose Different File…").
    var pendingInstallURL: URL?

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
    private var lastAutoDeviceRefresh: Date = .distantPast
    private var lifecycleObservers: [NSObjectProtocol] = []

    init() {
        clockTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(300))
                self?.clock = Date()
            }
        }
        client.onEvent = { [weak self] ev in Task { @MainActor in self?.apply(event: ev) } }
        client.onDisconnect = { [weak self] in Task { @MainActor in self?.disconnected() } }
        lifecycleObservers.append(
            NSWorkspace.shared.notificationCenter.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { [weak self] _ in
                Task { @MainActor in await self?.refreshDevicesIfStale(minInterval: 5) }
            }
        )
        lifecycleObservers.append(
            NotificationCenter.default.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
                Task { @MainActor in await self?.refreshDevicesIfStale(minInterval: 15) }
            }
        )
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
        guard let dir = stagingDir(path) else { return }
        try? FileManager.default.removeItem(at: dir)
    }

    /// The Incoming/<uuid>/ folder `stage` made for `path`, if it is one.
    nonisolated static func stagingDir(_ path: String) -> URL? {
        let incoming = dataDir.appendingPathComponent("Incoming").standardizedFileURL.path
        let dir = URL(fileURLWithPath: path).standardizedFileURL.deletingLastPathComponent()
        guard dir.deletingLastPathComponent().path == incoming, UUID(uuidString: dir.lastPathComponent) != nil else { return nil }
        return dir
    }

    /// Unstage, unless an installed app still refreshes from that copy
    /// (no library copy, or making one failed).
    func unstageIfUnused(_ path: String) {
        guard let dir = Self.stagingDir(path)?.path else { return }
        let used = apps.contains { a in
            ([a.sourcePath] + a.options.inject).contains { $0 == dir || $0.hasPrefix(dir + "/") }
        }
        if !used { Self.unstage(path) }
    }

    /// Advances every 5 minutes, for views whose state depends on the time
    /// (the menu bar icon's "expires soon" dot).
    var clock = Date()
    @ObservationIgnored private var clockTask: Task<Void, Never>?

    /// Staged files of install sheets that are still open (they may retry).
    var liveStaging: Set<String> = []
    /// job id → staged inputs to drop once the job is over.
    private var stagedForJob: [String: [String]] = [:]

    /// Drop a job's staged inputs when it ends: after success they're no
    /// longer needed; after a failure only if its sheet is gone (else the
    /// sheet may Try Again, and unstages when it closes).
    func unstageWhenDone(job id: String, paths: [String]) {
        stagedForJob[id] = paths
        if let j = job(id), !j.isActive { jobEnded(j) }
    }

    /// The install sheet took its staged files back (closed, or retrying).
    func forgetStaged(job id: String?) {
        if let id { stagedForJob[id] = nil }
    }

    private func jobEnded(_ j: JobInfo) {
        guard let paths = stagedForJob[j.id] else { return }
        if j.status != .succeeded && paths.contains(where: liveStaging.contains) { return }
        stagedForJob[j.id] = nil
        for p in paths { unstageIfUnused(p) }
    }

    /// Export results the app couldn't save where the user asked.
    var exportErrors: [String: String] = [:]

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
        let fm = FileManager.default
        // Copy next to the destination first, then swap it in: the old file
        // survives anything that fails along the way (a full disk, say).
        let partial = dest.deletingLastPathComponent().appendingPathComponent(".\(dest.lastPathComponent).\(UUID().uuidString.prefix(8)).partial")
        do {
            try fm.copyItem(at: src, to: partial)
            if fm.fileExists(atPath: dest.path) {
                _ = try fm.replaceItemAt(dest, withItemAt: partial)
            } else {
                try fm.moveItem(at: partial, to: dest)
            }
            try? fm.removeItem(at: src.deletingLastPathComponent())
            exportDestinations[job.id] = dest // remembered for "Show in Finder"
        } catch {
            try? fm.removeItem(at: partial)
            exportErrors[job.id] = "It's still at \(out). (\(error.localizedDescription))"
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
            // Already registered with launchd (e.g. still starting up at
            // login)? Then wait for it instead of reinstalling, which would
            // restart it.
            if !force, Self.daemonLoaded(), Self.agentProgram() == daemon.path {
                // Registered for this copy of Loadside, maybe stopped: start it
                // rather than reinstall (which would restart a running one).
                Self.kickstartDaemon()
                await MainActor.run { self.bootstrapping = false; self.triedInstallingAgent = false }
                return
            }
            let p = Process()
            p.executableURL = cli
            p.arguments = ["daemon", "install", "--bin", daemon.path]
            let errPipe = Pipe()
            p.standardError = errPipe
            p.standardOutput = FileHandle.nullDevice
            var err: ServiceError?
            do {
                try p.run()
                // Drain before waiting: a full pipe would block the child forever.
                let data = errPipe.fileHandleForReading.readDataToEndOfFile()
                p.waitUntilExit()
                let text = String(data: data, encoding: .utf8) ?? ""
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

    /// The daemon binary the LaunchAgent runs, if it's installed.
    nonisolated static func agentProgram() -> String? {
        let url = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/LaunchAgents/dev.loadside.daemon.plist")
        guard let d = NSDictionary(contentsOf: url), let args = d["ProgramArguments"] as? [String] else { return nil }
        return args.first
    }

    nonisolated static func kickstartDaemon() {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        p.arguments = ["kickstart", "gui/\(getuid())/dev.loadside.daemon"]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        guard (try? p.run()) != nil else { return }
        p.waitUntilExit()
    }

    /// Whether launchd has the background service loaded for this user.
    nonisolated static func daemonLoaded() -> Bool {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        p.arguments = ["print", "gui/\(getuid())/dev.loadside.daemon"]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        guard (try? p.run()) != nil else { return false }
        p.waitUntilExit()
        return p.terminationStatus == 0
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
        let before = (account?.email, account?.teamId)
        account = s.account
        loggedIn = s.loggedIn
        // Reconnected to a daemon on another Apple ID or team: drop its caches.
        if before != (account?.email, account?.teamId) { appIds = nil; certs = nil; teams = nil }
        devices = s.devices
        apps = s.apps
        jobs = s.jobs
        // Jobs that finished while we were disconnected.
        for j in jobs where !j.isActive {
            if j.status == .succeeded, j.kind == .export { finishExport(j) }
            jobEnded(j)
        }
        let hadPrompts = !prompts.isEmpty
        prompts = s.prompts
        if !prompts.isEmpty && !hadPrompts { bringMainWindowForward() }
        // Screenshot/QA aid: show a prompt sheet without a real sign-in.
        if let kind = ProcessInfo.processInfo.environment["LOADSIDE_DEMO_PROMPT"] {
            prompts.append(Prompt(id: "demo", kind: kind, payload: .object([:]), createdAt: 0))
        }
        config = s.config
        nextCheckAt = s.nextCheckAt
        githubTokenSet = s.githubTokenSet ?? false
    }

    private func apply(event ev: JSON) {
        switch ev["kind"]?.string {
        case "job":
            guard let j = try? ev["job"]?.decode(JobInfo.self) else { return }
            if let i = jobs.firstIndex(where: { $0.id == j.id }) { jobs[i] = j } else { jobs.append(j) }
            if j.status == .succeeded, j.kind == .install || j.kind == .refresh || j.kind == .update {
                // Account quotas changed.
                appIds = nil
                if account?.isFree == true { Task { await loadAppIds() } }
            }
            if j.status == .succeeded, j.kind == .export { finishExport(j) }
            if !j.isActive { jobEnded(j) }
        case "devices":
            if let d = try? ev["devices"]?.decode([DeviceInfo].self) {
                devices = d
                Task { await loadSetup() }
            }
        case "apps":
            if let a = try? ev["apps"]?.decode([AppRecord].self) { apps = a }
        case "account":
            let before = (account?.email, account?.teamId)
            account = try? ev["account"]?.decode(AccountInfo.self)
            loggedIn = ev["loggedIn"]?.bool ?? ev["logged_in"]?.bool ?? false
            // Another Apple ID or team: its App IDs, certificates and teams differ.
            if (!loggedIn && account == nil) || before != (account?.email, account?.teamId) {
                appIds = nil; certs = nil; teams = nil
                if account?.isFree == true { Task { await loadAppIds() } }
            }
            Task { await loadSetup() }
        case "prompt":
            if let p = try? ev["prompt"]?.decode(Prompt.self), !prompts.contains(where: { $0.id == p.id }) {
                prompts.append(p)
                // Prompts are shown by the main window. If it's closed (menu
                // bar only, or the user started this from Settings), open it.
                // Only user-started work prompts (the daemon never prompts for
                // background jobs), so bring the window forward right away.
                bringMainWindowForward()
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
        case "schedule":
            nextCheckAt = ev["next_check_at"]?.int ?? ev["nextCheckAt"]?.int
        case "config":
            if let c = try? ev["config"]?.decode(DaemonConfig.self) { config = c }
        case "github_token", "git_hub_token":
            githubTokenSet = ev["configured"]?.bool ?? false
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

    /// Sideloaded apps Loadside has on a device: still on it and not
    /// expired (an expired app no longer counts toward the limit).
    func activeApps(on udid: String) -> [AppRecord] {
        let now = Int(Date().timeIntervalSince1970)
        return apps.filter { $0.deviceUdid == udid && $0.onDevice != false && ($0.expiresAt ?? .max) > now }
    }

    /// Slots used on the connected device (else the device with the most apps).
    var slotsUsed: (device: String, used: Int)? {
        if let d = devices.first { return (d.name, activeApps(on: d.udid).count) }
        let udids = Set(apps.map(\.deviceUdid))
        guard let top = udids.max(by: { activeApps(on: $0).count < activeApps(on: $1).count }) else { return nil }
        return (apps.first { $0.deviceUdid == top }?.deviceName ?? "", activeApps(on: top).count)
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

    func refresh(_ app: AppRecord, skipUpdate: Bool = false) async {
        // A deleted app gets reinstalled; otherwise this is a normal refresh.
        let reinstall = app.onDevice == false
        await run { _ = try await client.call("refresh", .object(["app_id": .string(app.id), "reinstall": .bool(reinstall), "skip_update": .bool(skipUpdate)])) }
    }

    func refreshAll() async { await run { _ = try await client.call("refresh_all") } }

    func setAutoRefresh(_ app: AppRecord, _ on: Bool) async {
        await run { _ = try await client.call("update_app", .object(["app_id": .string(app.id), "auto_refresh": .bool(on)])) }
    }

    func remove(_ app: AppRecord, uninstall: Bool) async {
        await run { _ = try await client.call("remove_app", .object(["app_id": .string(app.id), "uninstall": .bool(uninstall)])) }
    }

    func refreshDevices() async {
        lastAutoDeviceRefresh = Date()
        if let d = await run({ try await client.call("devices", .object(["refresh": .bool(true)]), as: [DeviceInfo].self) }) { devices = d }
    }

    func refreshDevicesIfStale(minInterval: TimeInterval = 15) async {
        guard connection == .connected, Date().timeIntervalSince(lastAutoDeviceRefresh) >= minInterval else { return }
        lastAutoDeviceRefresh = Date()
        if let d = try? await client.call("devices", .object(["refresh": .bool(true)]), as: [DeviceInfo].self) {
            devices = d
        }
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

    // MARK: GitHub release tracking

    func githubInspect(repo: String, includePrereleases: Bool = false, assetPattern: String? = nil) async throws -> GitHubInspectResult {
        var params: [String: JSON] = [
            "repo": .string(repo),
            "include_prereleases": .bool(includePrereleases),
        ]
        if let assetPattern, !assetPattern.isEmpty {
            params["asset_pattern"] = .string(assetPattern)
        }
        return try await client.call("github_inspect", .object(params), as: GitHubInspectResult.self)
    }

    func githubDownload(
        repo: String,
        includePrereleases: Bool = false,
        assetPattern: String? = nil,
        assetName: String? = nil,
        releaseId: UInt64? = nil,
        assetId: UInt64? = nil
    ) async throws -> GitHubDownloadResult {
        var params: [String: JSON] = [
            "repo": .string(repo),
            "include_prereleases": .bool(includePrereleases),
        ]
        if let assetPattern, !assetPattern.isEmpty {
            params["asset_pattern"] = .string(assetPattern)
        }
        if let assetName, !assetName.isEmpty {
            params["asset_name"] = .string(assetName)
        }
        if let releaseId { params["release_id"] = .number(Double(releaseId)) }
        if let assetId { params["asset_id"] = .number(Double(assetId)) }
        return try await client.call("github_download", .object(params), as: GitHubDownloadResult.self)
    }

    func setAppGitHubThrowing(
        _ app: AppRecord,
        disconnect: Bool = false,
        repo: String? = nil,
        policy: UpdatePolicy? = nil,
        includePrereleases: Bool? = nil,
        assetPattern: String?? = nil
    ) async throws {
        var params: [String: JSON] = ["app_id": .string(app.id), "disconnect": .bool(disconnect)]
        if let repo { params["repo"] = .string(repo) }
        if let policy { params["policy"] = .string(policy.rawValue) }
        if let includePrereleases { params["include_prereleases"] = .bool(includePrereleases) }
        if let assetPattern {
            params["asset_pattern"] = assetPattern.map(JSON.string) ?? .null
        }
        _ = try await client.call("set_app_github", .object(params))
    }

    @discardableResult
    func setAppGitHub(
        _ app: AppRecord,
        disconnect: Bool = false,
        repo: String? = nil,
        policy: UpdatePolicy? = nil,
        includePrereleases: Bool? = nil,
        assetPattern: String?? = nil
    ) async -> Bool {
        await run {
            try await setAppGitHubThrowing(
                app,
                disconnect: disconnect,
                repo: repo,
                policy: policy,
                includePrereleases: includePrereleases,
                assetPattern: assetPattern
            )
        } != nil
    }

    @discardableResult
    func checkUpdates(app: AppRecord? = nil, force: Bool = true) async -> CheckUpdatesResult? {
        var params: [String: JSON] = ["force": .bool(force)]
        if let app { params["app_id"] = .string(app.id) }
        return await run { try await client.call("check_updates", .object(params), as: CheckUpdatesResult.self) }
    }

    func updateRelease(_ app: AppRecord) async {
        await run { _ = try await client.call("update_release", .object(["app_id": .string(app.id)])) }
    }

    @discardableResult
    func setGitHubToken(_ token: String?) async -> Bool {
        let val: JSON = token.map(JSON.string) ?? .null
        return await run { _ = try await client.call("set_github_token", .object(["token": val])) } != nil
    }

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
