import SwiftUI
import UniformTypeIdentifiers

/// A file the user picked or dropped, waiting in the install sheet.
struct PendingInstall: Identifiable {
    let id = UUID()
    let url: URL
}

/// The whole app in one window: device status, your apps, and a way to add more.
struct ContentView: View {
    @Environment(Store.self) private var store
    @Environment(\.openSettings) private var openSettings
    @Environment(\.openWindow) private var openWindow
    @State private var pending: PendingInstall?
    @State private var showSetup = false
    @State private var detailAppId: String?
    @State private var dropTargeted = false

    static let ipaType = UTType(filenameExtension: "ipa") ?? .data

    private var anySheetOpen: Bool { pending != nil || showSetup || detailAppId != nil }

    var body: some View {
        Group {
            switch store.connection {
            case .connected:
                HomeView(add: chooseFile, openSetup: { showSetup = true }, openApp: { detailAppId = $0.id })
            case .connecting:
                ProgressView("Connecting to the Loadside service…").frame(maxWidth: .infinity, maxHeight: .infinity)
            case .unavailable(let why):
                DaemonUnavailableView(reason: why)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .overlay(alignment: .top) { BannerView() }
        .overlay { if dropTargeted { DropOverlay() } }
        .onDrop(of: [.fileURL], isTargeted: $dropTargeted, perform: handleDrop)
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                ActivityButton()
                Button { openSettings() } label: { Label("Settings", systemImage: "gearshape") }
                    .help("Settings")
                Button(action: chooseFile) { Label("Add App", systemImage: "plus") }
                    .help("Install an .ipa (⌘O)")
                    .disabled(store.connection != .connected)
            }
        }
        .sheet(item: $pending) { p in
            InstallSheet(source: p.url).environment(store)
        }
        .sheet(isPresented: $showSetup) {
            SetupSheet().environment(store)
        }
        .sheet(item: Binding(get: { detailAppId.map(IdBox.init) }, set: { detailAppId = $0?.id })) { box in
            AppDetailSheet(appId: box.id).environment(store)
        }
        .sheet(item: Binding(get: { store.showOnboarding || anySheetOpen ? nil : store.prompts.first }, set: { _ in })) { prompt in
            PromptSheet(prompt: prompt).id(prompt.id)
                .environment(store)
                .interactiveDismissDisabled()
        }
        .sheet(isPresented: Binding(get: { store.showOnboarding && store.connection == .connected && !anySheetOpen }, set: { store.showOnboarding = $0 })) {
            OnboardingView()
                .environment(store)
                .overlay(alignment: .top) { BannerView() }
        }
        .onChange(of: store.navigate) { _, target in handle(target) }
        .onAppear {
            handle(store.navigate)
            AppDelegate.openMainWindow = { openWindow(id: "main") }
        }
        // Double-clicked IPA / "Open With Loadside" (into this window, not a new one).
        .handlesExternalEvents(preferring: ["*"], allowing: ["*"])
        .onOpenURL { url in
            guard url.isFileURL, ["ipa", "app"].contains(url.pathExtension.lowercased()) else { return }
            openInstall(url)
        }
        .onChange(of: store.pendingInstallURL) { _, url in
            guard let url else { return }
            store.pendingInstallURL = nil
            openInstall(url)
        }
    }

    private func handle(_ target: String?) {
        guard let target else { return }
        store.navigate = nil
        switch target {
        case "install":
            store.showOnboarding = false
            chooseFile()
        case "setup":
            store.showOnboarding = false
            showSetup = true
        case "account":
            // Signed out: the sign-in form is right on the home screen.
            if store.account == nil {
                showSetup = false
                store.showOnboarding = false
            } else {
                openSettingsTab("account")
            }
        case "settings", "devices", "advanced", "general":
            openSettingsTab(target == "settings" ? "general" : target)
        default:
            break
        }
    }

    private func openSettingsTab(_ tab: String) {
        UserDefaults.standard.set(tab, forKey: SettingsView.tabKey)
        openSettings()
    }

    private func chooseFile() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [Self.ipaType, .applicationBundle]
        panel.allowsMultipleSelection = false
        panel.message = "Choose an app to install"
        panel.prompt = "Choose"
        if panel.runModal() == .OK, let url = panel.url { openInstall(url) }
    }

    /// Show the install sheet, closing whatever sheet is up first (SwiftUI
    /// drops a second sheet presented on the same view).
    private func openInstall(_ url: URL) {
        AppDelegate.startHidden = false
        guard anySheetOpen || store.showOnboarding else {
            pending = PendingInstall(url: url)
            return
        }
        pending = nil
        showSetup = false
        detailAppId = nil
        store.showOnboarding = false
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { pending = PendingInstall(url: url) }
    }

    private func handleDrop(_ providers: [NSItemProvider]) -> Bool {
        guard let p = providers.first else { return false }
        _ = p.loadObject(ofClass: URL.self) { url, _ in
            guard let url else { return }
            Task { @MainActor in
                let ext = url.pathExtension.lowercased()
                if ext == "ipa" || ext == "app" {
                    openInstall(url)
                } else {
                    store.show(Failure.local("That's not an app.", hint: "Drop an .ipa file to install it."))
                }
            }
        }
        return true
    }
}

private struct IdBox: Identifiable { let id: String }

private struct DropOverlay: View {
    var body: some View {
        ZStack {
            Rectangle().fill(.ultraThinMaterial)
            RoundedRectangle(cornerRadius: 26, style: .continuous)
                .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 2.5, dash: [10, 8]))
                .padding(18)
            VStack(spacing: 10) {
                Image(systemName: "arrow.down.app.fill").font(.system(size: 52)).foregroundStyle(.tint)
                Text("Drop to install").font(.title2.weight(.semibold))
            }
        }
        .allowsHitTesting(false)
        .transition(.opacity)
    }
}

// MARK: - Home

struct HomeView: View {
    @Environment(Store.self) private var store
    let add: () -> Void
    let openSetup: () -> Void
    let openApp: (AppRecord) -> Void

    private let columns = [GridItem(.adaptive(minimum: 230, maximum: 340), spacing: 16)]

    var body: some View {
        // Re-render every minute so "N days left" and "next check" stay true.
        TimelineView(.periodic(from: .now, by: 60)) { _ in content }
            .task(id: store.account?.email) { await store.ensureQuota() }
    }

    private var content: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                StatusStrip()
                attention
                if store.account == nil {
                    SignInPanel()
                } else if store.apps.isEmpty {
                    EmptyLibrary(add: add)
                } else {
                    appsSection
                }
            }
            .padding(.horizontal, 28)
            .padding(.top, 14)
            .padding(.bottom, 28)
            .frame(maxWidth: 1000)
            .frame(maxWidth: .infinity)
        }
    }

    /// One line about the most important unfinished setup step.
    @ViewBuilder private var attention: some View {
        if let r = store.setup {
            // Signed out is handled by the sign-in panel below.
            let blocking = r.checks.filter { $0.isBlocking && !($0.id == "account" && store.account == nil) }
            if let first = blocking.first {
                Button(action: openSetup) {
                    HStack(spacing: 12) {
                        Image(systemName: "exclamationmark.circle.fill").font(.title2).foregroundStyle(.orange)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(first.title).font(.callout.weight(.semibold))
                            Text(first.summary).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                        }
                        Spacer()
                        Text(blocking.count > 1 ? "Fix \(blocking.count) issues" : "Fix")
                            .font(.callout.weight(.medium)).foregroundStyle(.tint)
                        Image(systemName: "chevron.right").font(.caption.weight(.semibold)).foregroundStyle(.tertiary)
                    }
                    .padding(14)
                    .background(Color.orange.opacity(0.10), in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
    }

    private var appsSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline) {
                Text("Apps").font(.title2.weight(.bold))
                Spacer()
                FreeQuotaView(compact: true)
            }
            LazyVGrid(columns: columns, alignment: .leading, spacing: 16) {
                ForEach(sortedApps) { app in
                    AppCard(app: app, onOpen: { openApp(app) })
                }
                ForEach(0..<openSlots, id: \.self) { i in
                    AddAppCard(add: add, emphasized: i == 0)
                }
            }
        }
    }

    private var sortedApps: [AppRecord] {
        store.apps.sorted { ($0.expiresAt ?? .max) < ($1.expiresAt ?? .max) }
    }

    /// Free accounts get 3 apps per device: show the free ones as drop targets.
    private var openSlots: Int {
        guard store.account?.isFree == true, let left = store.freeSlotsLeft else { return 1 }
        // Always leave somewhere to drop an app (apps from other tools aren't counted).
        return max(1, left)
    }
}

/// Device + auto-refresh at a glance.
struct StatusStrip: View {
    @Environment(Store.self) private var store
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        HStack(spacing: 10) {
            if store.devices.isEmpty {
                DisconnectedPill(name: lastDeviceName)
            } else {
                ForEach(store.devices) { d in DevicePill(device: d) }
            }
            Spacer(minLength: 8)
            Button {
                UserDefaults.standard.set("general", forKey: SettingsView.tabKey)
                openSettings()
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: autoOn ? "arrow.triangle.2.circlepath" : "pause.circle")
                        .foregroundStyle(autoOn ? .green : .orange)
                    Text(autoText).foregroundStyle(.secondary)
                }
                .font(.caption)
            }
            .buttonStyle(.plain)
            .help("Automatic refresh settings")
        }
    }

    private var autoOn: Bool { store.config?.autoRefresh == true }
    private var autoText: String {
        guard autoOn else { return "Auto-refresh off" }
        if let n = store.nextCheckAt { return "Auto-refresh on · next check \(Fmt.rel(n))" }
        return "Auto-refresh on"
    }
    private var lastDeviceName: String? {
        store.apps.map(\.deviceName).first { !$0.isEmpty }
    }
}

struct DevicePill: View {
    let device: DeviceInfo
    @State private var showInfo = false

    var body: some View {
        Button { showInfo.toggle() } label: {
            HStack(spacing: 8) {
                Image(systemName: device.model?.hasPrefix("iPad") == true ? "ipad" : "iphone")
                Text(device.name).fontWeight(.medium)
                HStack(spacing: 4) {
                    StatusDot(color: device.problem == nil ? .green : .orange)
                    Text(device.problem == nil ? device.connectionLabel : "Needs attention")
                        .foregroundStyle(.secondary)
                }
            }
            .font(.callout)
            .padding(.horizontal, 12).padding(.vertical, 7)
            .background(Capsule().fill(Color.primary.opacity(0.06)))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .popover(isPresented: $showInfo, arrowEdge: .bottom) {
            DeviceDetails(device: device).padding(16).frame(width: 340)
        }
    }
}

struct DisconnectedPill: View {
    @Environment(Store.self) private var store
    let name: String?
    @State private var showHelp = false
    @State private var scanning = false

    var body: some View {
        Button { showHelp.toggle() } label: {
            HStack(spacing: 8) {
                Image(systemName: "iphone.slash")
                Text(name ?? "No device").fontWeight(.medium)
                HStack(spacing: 4) {
                    StatusDot(color: .secondary)
                    Text("Not connected").foregroundStyle(.secondary)
                }
            }
            .font(.callout)
            .padding(.horizontal, 12).padding(.vertical, 7)
            .background(Capsule().fill(Color.primary.opacity(0.06)))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .popover(isPresented: $showHelp, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 10) {
                Text("Connect your \(name == nil ? "iPhone or iPad" : "device")").font(.headline)
                Text("Unlock it and keep it on the same Wi-Fi as this Mac. Or plug in the cable and tap Trust if asked.")
                    .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Text("A locked iPhone drops off Wi-Fi; it comes back when you unlock it.")
                    .font(.caption).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
                Button {
                    Task { scanning = true; await store.refreshDevices(); scanning = false }
                } label: {
                    HStack(spacing: 6) {
                        if scanning { ProgressView().controlSize(.small) }
                        Text("Look Again")
                    }
                }
                .disabled(scanning)
            }
            .padding(16).frame(width: 300)
        }
    }
}

// MARK: - App cards

struct AppCard: View {
    @Environment(Store.self) private var store
    let app: AppRecord
    let onOpen: () -> Void
    @State private var hover = false

    private var job: JobInfo? { store.jobs.last { $0.appId == app.id && $0.isActive } }
    private var deviceConnected: Bool { store.devices.contains { $0.udid == app.deviceUdid } }

    var body: some View {
        let expiry = Expiry.of(app)
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: 14) {
                AppIconView(path: app.sourcePath, name: app.name, size: 58)
                VStack(alignment: .leading, spacing: 2) {
                    Text(app.name).font(.headline).lineLimit(1)
                    Text([app.version, app.deviceName.isEmpty ? nil : app.deviceName].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                status(expiry)
                    .frame(minHeight: 38, alignment: .top)
            }
            .surface(cornerRadius: 20, padding: 16)
            .overlay(RoundedRectangle(cornerRadius: 20, style: .continuous).strokeBorder(Color.accentColor.opacity(hover ? 0.35 : 0)))
            .contentShape(RoundedRectangle(cornerRadius: 20, style: .continuous))
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(app.name), \(expiry.text)")
        .overlay(alignment: .topTrailing) { refreshButton.padding(16) }
        .scaleEffect(hover ? 1.01 : 1)
        .animation(.easeOut(duration: 0.15), value: hover)
        .onHover { hover = $0 }
        .contextMenu {
            Button(app.onDevice == false ? "Reinstall" : "Refresh Now") { Task { await store.refresh(app) } }
                .disabled(job != nil || !deviceConnected)
            Toggle("Refresh Automatically", isOn: Binding(get: { app.options.autoRefresh }, set: { v in Task { await store.setAutoRefresh(app, v) } }))
            Divider()
            Button("Details…", action: onOpen)
            Button("Show Original IPA in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: app.sourcePath)]) }
        }
    }

    private var refreshButton: some View {
        Button { Task { await store.refresh(app) } } label: {
            Image(systemName: app.onDevice == false ? "arrow.down" : "arrow.clockwise")
                .font(.callout.weight(.semibold))
                .frame(width: 30, height: 30)
                .background(Circle().fill(Color.primary.opacity(hover ? 0.1 : 0.06)))
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(job != nil || !deviceConnected)
        .opacity(job != nil || !deviceConnected ? 0.35 : 1)
        .help(deviceConnected ? (app.onDevice == false ? "Reinstall" : "Refresh now (keeps app data)") : "Connect \(app.deviceName.isEmpty ? "the device" : app.deviceName) first")
    }

    @ViewBuilder private func status(_ expiry: Expiry) -> some View {
        if let job {
            JobProgressView(job: job)
        } else if app.onDevice == false {
            VStack(alignment: .leading, spacing: 4) {
                Label("Deleted from device", systemImage: "trash").font(.callout.weight(.medium)).foregroundStyle(.secondary)
                Text("Not refreshed until you reinstall it.").font(.caption).foregroundStyle(.tertiary)
            }
        } else if let e = app.lastError {
            VStack(alignment: .leading, spacing: 4) {
                Label(expiry.text, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout.weight(.semibold)).foregroundStyle(.orange)
                Text(e.message).font(.caption).foregroundStyle(.secondary).lineLimit(2)
            }
        } else {
            VStack(alignment: .leading, spacing: 7) {
                HStack(alignment: .firstTextBaseline) {
                    Text(expiry.text).font(.callout.weight(.semibold))
                        .foregroundStyle(expiry.color == .green ? Color.primary : (expiry.color == .yellow ? .orange : expiry.color))
                    Spacer()
                    Text(app.options.autoRefresh ? "Auto" : "Manual")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(app.options.autoRefresh ? Color.green : Color.orange)
                        .help(app.options.autoRefresh ? "Refreshed automatically before it expires" : "Auto-refresh is off for this app")
                }
                ExpiryBar(expiry: expiry)
            }
        }
    }
}

struct AddAppCard: View {
    let add: () -> Void
    var emphasized = true
    @State private var hover = false

    var body: some View {
        Button(action: add) {
            VStack(spacing: 8) {
                Image(systemName: "plus").font(.system(size: 22, weight: .medium))
                    .frame(width: 44, height: 44)
                    .background(Circle().fill(Color.accentColor.opacity(hover ? 0.2 : 0.12)))
                    .foregroundStyle(.tint)
                Text(emphasized ? "Add an app" : "Free slot").font(.callout.weight(.medium))
                    .foregroundStyle(emphasized ? .primary : .secondary)
                if emphasized {
                    Text("Drop an .ipa or click").font(.caption).foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity, minHeight: 170)
            .background(
                RoundedRectangle(cornerRadius: 20, style: .continuous)
                    .strokeBorder(Color.primary.opacity(hover ? 0.25 : 0.12), style: StrokeStyle(lineWidth: 1.5, dash: [6, 5]))
            )
            .contentShape(RoundedRectangle(cornerRadius: 20, style: .continuous))
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
    }
}

struct EmptyLibrary: View {
    let add: () -> Void
    var body: some View {
        Button(action: add) {
            VStack(spacing: 14) {
                Image(systemName: "arrow.down.app")
                    .font(.system(size: 46, weight: .light))
                    .foregroundStyle(.tint)
                Text("Install your first app").font(.title2.weight(.semibold))
                Text("Drop an .ipa anywhere in this window, or click to choose one.\nLoadside signs it, installs it and keeps it fresh in the background.")
                    .multilineTextAlignment(.center)
                    .font(.callout).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, minHeight: 300)
            .background(
                RoundedRectangle(cornerRadius: 26, style: .continuous)
                    .strokeBorder(Color.primary.opacity(0.14), style: StrokeStyle(lineWidth: 1.5, dash: [8, 6]))
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

// MARK: - Sign in

struct SignInPanel: View {
    var body: some View {
        VStack(spacing: 18) {
            Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 76, height: 76)
            VStack(spacing: 6) {
                Text("Sign in to get started").font(.title2.weight(.semibold))
                Text("Loadside signs apps with your Apple ID. A free one works.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            LoginForm()
                .frame(width: 340)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 30)
    }
}

// MARK: - Activity

struct ActivityButton: View {
    @Environment(Store.self) private var store
    @Environment(\.openWindow) private var openWindow
    @State private var show = false

    var body: some View {
        Button { show.toggle() } label: {
            if store.activeJobs.isEmpty {
                Label("Activity", systemImage: "clock.arrow.circlepath")
            } else {
                ProgressView().controlSize(.small)
            }
        }
        .help("Activity")
        .popover(isPresented: $show, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    Text("Activity").font(.headline)
                    Spacer()
                    Button("Clear") { Task { await store.clearJobs() } }
                        .buttonStyle(.borderless).disabled(store.jobs.allSatisfy(\.isActive))
                    Button("Logs") { show = false; openWindow(id: "logs") }.buttonStyle(.borderless)
                }
                .padding(14)
                Divider()
                if store.jobs.isEmpty {
                    Text("Nothing yet").foregroundStyle(.secondary).frame(maxWidth: .infinity).padding(30)
                } else {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 0) {
                            ForEach(store.jobs.reversed().prefix(30)) { j in
                                JobRow(job: j)
                                Divider().padding(.leading, 40)
                            }
                        }
                    }
                    .frame(maxHeight: 380)
                }
            }
            .frame(width: 380)
        }
    }
}

struct JobRow: View {
    @Environment(Store.self) private var store
    let job: JobInfo
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            icon.frame(width: 18).padding(.top, 1)
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Text(job.title).font(.callout.weight(.medium)).lineLimit(1)
                    if job.background { Tag(text: "auto") }
                    Spacer()
                    Text(Fmt.rel(job.finishedAt ?? job.createdAt)).font(.caption).foregroundStyle(.tertiary)
                }
                if job.isActive { JobProgressView(job: job) }
                if let e = job.error {
                    Text(e.message).font(.caption).foregroundStyle(.orange)
                    if let h = e.hint { Text(h).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
                }
                ForEach(job.warnings, id: \.self) { Text($0).font(.caption).foregroundStyle(.orange) }
                if job.status == .queued {
                    Button("Cancel") { Task { await store.cancel(job) } }.controlSize(.small)
                }
            }
        }
        .padding(.horizontal, 14).padding(.vertical, 10)
    }

    @ViewBuilder private var icon: some View {
        switch job.status {
        case .queued: Image(systemName: "clock").foregroundStyle(.secondary)
        case .running: ProgressView().controlSize(.mini)
        case .succeeded: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case .failed: Image(systemName: "xmark.circle.fill").foregroundStyle(.red)
        case .cancelled: Image(systemName: "minus.circle").foregroundStyle(.secondary)
        }
    }
}

// MARK: - Service unavailable / banners

struct DaemonUnavailableView: View {
    @Environment(Store.self) private var store
    let reason: String
    @State private var starting = false

    var body: some View {
        ContentUnavailableView {
            Label("Background service not running", systemImage: "bolt.horizontal.circle")
        } description: {
            VStack(spacing: 10) {
                Text(store.serviceError?.message ?? reason)
                    .textSelection(.enabled)
                Text(explanation).font(.callout).foregroundStyle(.secondary)
            }
            .frame(maxWidth: 520)
        } actions: {
            VStack(spacing: 10) {
                HStack {
                    if store.serviceError?.code == "login_item_disabled" {
                        Button("Open Login Items Settings") {
                            NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!)
                        }
                        .primaryButton()
                    }
                    let start = Button(starting ? "Starting…" : "Start Service") {
                        starting = true
                        store.serviceError = nil
                        store.bootstrapDaemon(force: true)
                        store.start()
                        Task { try? await Task.sleep(for: .seconds(3)); starting = false }
                    }
                    .disabled(starting)
                    if store.serviceError?.code == "login_item_disabled" { start.secondaryButton() } else { start.primaryButton() }
                    Button("Show Log") {
                        let log = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Loadside/loadsided.log")
                        if FileManager.default.fileExists(atPath: log.path) { NSWorkspace.shared.activateFileViewerSelecting([log]) }
                        else { NSWorkspace.shared.open(log.deletingLastPathComponent()) }
                    }
                    .secondaryButton()
                }
                if Store.bundledCLI == nil {
                    Text("Development build: run `loadside daemon install` in Terminal.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var explanation: String {
        switch store.serviceError?.code {
        case "login_item_disabled":
            "macOS is blocking it. In System Settings → General → Login Items & Extensions, turn on \"loadsided\" under Allow in the Background, then click Start Service."
        case "missing_binary":
            "The app bundle is incomplete. Download or build Loadside again and move it to Applications."
        case "not_responding":
            "It started but didn't answer. The log usually says why."
        default:
            "Loadside's background service installs and auto-refreshes apps, even when this window is closed. It uses about 15 MB of memory."
        }
    }
}

struct BannerView: View {
    @Environment(Store.self) private var store
    var body: some View {
        if let b = store.banner {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: b.isError ? "exclamationmark.triangle.fill" : "checkmark.circle.fill")
                    .foregroundStyle(b.isError ? .orange : .green)
                VStack(alignment: .leading, spacing: 2) {
                    Text(b.title).font(.callout.weight(.semibold))
                    if !b.message.isEmpty { Text(b.message).font(.caption).foregroundStyle(.secondary) }
                }
                Spacer()
                Button { store.banner = nil } label: { Image(systemName: "xmark").foregroundStyle(.secondary) }.buttonStyle(.plain)
            }
            .padding(14)
            .floatingGlass(cornerRadius: 16)
            .padding()
            .frame(maxWidth: 560)
            .transition(.move(edge: .top).combined(with: .opacity))
            .task(id: b.id) {
                try? await Task.sleep(for: .seconds(b.isError ? 12 : 5))
                if store.banner?.id == b.id { withAnimation { store.banner = nil } }
            }
        }
    }
}
