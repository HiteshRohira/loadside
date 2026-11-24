import SwiftUI

enum SidebarItem: String, CaseIterable, Identifiable {
    case install, apps, devices, account, setup, activity, settings
    var id: String { rawValue }
    var title: String {
        switch self {
        case .install: "Install"
        case .apps: "Apps"
        case .devices: "Devices"
        case .account: "Account"
        case .setup: "Setup"
        case .activity: "Activity"
        case .settings: "Settings"
        }
    }
    var icon: String {
        switch self {
        case .install: "square.and.arrow.down.on.square"
        case .apps: "square.grid.2x2"
        case .devices: "iphone"
        case .account: "person.crop.circle"
        case .setup: "checklist"
        case .activity: "list.bullet.rectangle"
        case .settings: "gearshape"
        }
    }
}

struct ContentView: View {
    @Environment(Store.self) private var store
    @State private var section: SidebarItem = SidebarItem(rawValue: ProcessInfo.processInfo.environment["LOADSIDE_SECTION"] ?? "") ?? .install

    var body: some View {
        NavigationSplitView {
            List(SidebarItem.allCases, selection: $section) { s in
                Label(s.title, systemImage: s.icon)
                    .badge(badge(for: s))
                    .tag(s)
            }
            .navigationSplitViewColumnWidth(min: 170, ideal: 190)
            .safeAreaInset(edge: .bottom) { SidebarFooter() }
        } detail: {
            Group {
                switch store.connection {
                case .connected:
                    detail
                case .connecting:
                    ProgressView("Connecting to the Loadside service…").frame(maxWidth: .infinity, maxHeight: .infinity)
                case .unavailable(let why):
                    DaemonUnavailableView(reason: why)
                }
            }
            .overlay(alignment: .top) { BannerView() }
        }
        .sheet(item: Binding(get: { store.showOnboarding ? nil : store.prompts.first }, set: { _ in })) { prompt in
            PromptSheet(prompt: prompt)
                .environment(store)
                .interactiveDismissDisabled()
        }
        .sheet(isPresented: Binding(get: { store.showOnboarding && store.connection == .connected }, set: { store.showOnboarding = $0 })) {
            OnboardingView()
                .environment(store)
                .overlay(alignment: .top) { BannerView() }
        }
        .onChange(of: store.navigate) { _, target in
            guard let target, let item = SidebarItem(rawValue: target) else { return }
            store.showOnboarding = false
            section = item
            store.navigate = nil
        }
    }

    @ViewBuilder private var detail: some View {
        switch section {
        case .install: InstallView(goToAccount: { section = .account })
        case .apps: AppsView()
        case .devices: DevicesView()
        case .account: AccountView()
        case .setup: SetupView()
        case .activity: ActivityView()
        case .settings: SettingsView()
        }
    }

    private func badge(for s: SidebarItem) -> Int {
        switch s {
        case .apps:
            let soon = Int(Date().timeIntervalSince1970) + 24 * 3600
            return store.apps.filter { ($0.expiresAt ?? .max) < soon || $0.lastError != nil }.count
        case .activity: return store.activeJobs.count
        case .setup: return store.setup?.blockingCount ?? 0
        default: return 0
        }
    }
}

struct SidebarFooter: View {
    @Environment(Store.self) private var store
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Divider()
            HStack(spacing: 6) {
                Circle().fill(store.connection == .connected ? .green : .orange).frame(width: 7, height: 7)
                Text(store.connection == .connected ? "Service running" : "Service offline")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let a = store.account {
                Text(a.email).font(.caption2).foregroundStyle(.tertiary).lineLimit(1).truncationMode(.middle)
            }
        }
        .padding(.horizontal, 12).padding(.bottom, 10)
    }
}

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
                        .buttonStyle(.borderedProminent)
                    }
                    let start = Button(starting ? "Starting…" : "Start Service") {
                        starting = true
                        store.serviceError = nil
                        store.bootstrapDaemon(force: true)
                        store.start()
                        Task { try? await Task.sleep(for: .seconds(3)); starting = false }
                    }
                    .disabled(starting)
                    if store.serviceError?.code == "login_item_disabled" { start.buttonStyle(.bordered) } else { start.buttonStyle(.borderedProminent) }
                    Button("Show Log") {
                        let log = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Loadside/loadsided.log")
                        if FileManager.default.fileExists(atPath: log.path) { NSWorkspace.shared.activateFileViewerSelecting([log]) }
                        else { NSWorkspace.shared.open(log.deletingLastPathComponent()) }
                    }
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
                Button { store.banner = nil } label: { Image(systemName: "xmark") }.buttonStyle(.plain)
            }
            .padding(12)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
            .shadow(radius: 6, y: 2)
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
