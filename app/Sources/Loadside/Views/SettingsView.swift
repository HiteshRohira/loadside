import SwiftUI

/// The standard Settings window (⌘,).
struct SettingsView: View {
    static let tabKey = "settingsTab"
    @Environment(Store.self) private var store
    @Environment(\.openWindow) private var openWindow
    @AppStorage(SettingsView.tabKey) private var tab = "general"

    var body: some View {
        TabView(selection: $tab) {
            GeneralSettings()
                .tabItem { Label("General", systemImage: "gearshape") }.tag("general")
            AccountSettings()
                .tabItem { Label("Account", systemImage: "person.crop.circle") }.tag("account")
            DevicesSettings()
                .tabItem { Label("Devices", systemImage: "iphone") }.tag("devices")
            AdvancedSettings()
                .tabItem { Label("Advanced", systemImage: "wrench.and.screwdriver") }.tag("advanced")
        }
        .frame(width: 580, height: 520)
        // With the menu bar icon off this may be the only window left; it
        // must still be able to reopen the main one (prompts, Setup Guide).
        .task { AppDelegate.openMainWindow = { openWindow(id: "main") } }
        .overlay {
            if store.connection != .connected {
                ContentUnavailableView("Background service not running", systemImage: "bolt.horizontal.circle",
                                       description: Text("Open the main window to start it."))
                    .background(.background)
            }
        }
    }
}

@MainActor private func configBinding(_ store: Store, _ kp: KeyPath<DaemonConfig, Bool>, _ key: String) -> Binding<Bool> {
    Binding(get: { store.config?[keyPath: kp] ?? false }, set: { v in Task { await store.setConfig([key: .bool(v)]) } })
}

struct GeneralSettings: View {
    @Environment(Store.self) private var store
    @AppStorage(Prefs.menuBar) private var showMenuBar = true
    @State private var checking = false
    @State private var loginItemError: String?

    var body: some View {
        Form {
            if let c = store.config {
                Section {
                    Toggle("Refresh apps automatically", isOn: configBinding(store, \.autoRefresh, "auto_refresh"))
                    Picker("Refresh when", selection: Binding(get: { c.refreshThresholdHours }, set: { v in Task { await store.setConfig(["refresh_threshold_hours": .number(Double(v))]) } })) {
                        ForEach(Array(Set([24, 48, 72, 96, 120, c.refreshThresholdHours])).sorted(), id: \.self) { h in
                            Text(h % 24 == 0 ? "\(h / 24) day\(h == 24 ? "" : "s") left" : "\(h) hours left").tag(h)
                        }
                    }
                    .disabled(!c.autoRefresh)
                    Picker("Check every", selection: Binding(get: { c.checkIntervalMinutes }, set: { v in Task { await store.setConfig(["check_interval_minutes": .number(Double(v))]) } })) {
                        ForEach(Array(Set([15, 30, 60, 120, 240, c.checkIntervalMinutes])).sorted(), id: \.self) { m in
                            Text(m < 60 ? "\(m) minutes" : "\(m / 60) hour\(m == 60 ? "" : "s")").tag(m)
                        }
                    }
                    .disabled(!c.autoRefresh)
                    LabeledContent("Next check") {
                        HStack {
                            TimelineView(.periodic(from: .now, by: 30)) { _ in
                                Text(store.nextCheckAt.map { Fmt.rel($0) } ?? "—").foregroundStyle(.secondary)
                            }
                            Button(checking ? "Checking…" : "Check Now") {
                                Task { checking = true; await store.checkNow(); try? await Task.sleep(for: .seconds(1)); checking = false }
                            }
                            .disabled(checking)
                        }
                    }
                } header: {
                    Text("Automatic refresh")
                } footer: {
                    Footnote("Loadside also checks the moment your device connects over USB or Wi-Fi. If a refresh fails it tries again later (30 minutes, then less often).")
                }
                Section("Notifications") {
                    Toggle("Notify when apps refresh or fail", isOn: configBinding(store, \.notifications, "notifications"))
                }
            }
            Section {
                Toggle("Show Loadside in the menu bar", isOn: $showMenuBar)
                if let err = loginItemError {
                    Label(err, systemImage: "exclamationmark.triangle.fill").font(.callout).foregroundStyle(.orange)
                }
            } header: {
                Text("Menu bar")
            } footer: {
                Footnote("Loadside starts in the menu bar when you log in, so you can always see the background service at work. Turn this off to hide it; refreshing continues either way.")
            }
        }
        .formStyle(.grouped)
        .onAppear { loginItemError = LoginItem.lastError }
        .onChange(of: showMenuBar) { _, on in
            LoginItem.sync(enabled: on)
            loginItemError = LoginItem.lastError
        }
    }
}

struct DevicesSettings: View {
    @Environment(Store.self) private var store
    @State private var scanning = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                HStack {
                    Text("Connected devices").font(.headline)
                    Spacer()
                    Button {
                        Task { scanning = true; await store.refreshDevices(); scanning = false }
                    } label: { Label("Look Again", systemImage: "arrow.clockwise") }
                        .disabled(scanning)
                }
                if store.devices.isEmpty {
                    Text("None right now. Unlock your iPhone on the same Wi-Fi as this Mac, or connect the cable.")
                        .font(.callout).foregroundStyle(.secondary)
                        .surface(cornerRadius: 12, padding: 14)
                }
                ForEach(store.devices) { d in
                    DeviceDetails(device: d).surface(cornerRadius: 12, padding: 14)
                }
                HStack {
                    Text("Setup checklist").font(.headline)
                    Spacer()
                    Button("Open Setup Guide") { store.reopenOnboarding(); bringMainWindowForward() }
                }
                .padding(.top, 6)
                SetupChecklist()
            }
            .padding(20)
        }
        .task {
            while !Task.isCancelled {
                await store.loadSetup()
                try? await Task.sleep(for: .seconds(6))
            }
        }
    }
}

struct AdvancedSettings: View {
    @Environment(Store.self) private var store
    @Environment(\.openWindow) private var openWindow
    @State private var anisette = ""
    @State private var machineName = ""
    @State private var confirmReset = false
    @State private var memory = "—"

    var body: some View {
        Form {
            if let c = store.config {
                Section {
                    Toggle("Revoke old Loadside certificates automatically", isOn: configBinding(store, \.autoRevokeOwnCerts, "auto_revoke_own_certs"))
                    HStack {
                        TextField("Certificate name", text: $machineName).onSubmit(saveMachineName)
                        if machineName != c.machineName {
                            Button("Save", action: saveMachineName).disabled(machineName.trimmingCharacters(in: .whitespaces).isEmpty)
                        }
                    }
                } header: {
                    Text("Signing")
                } footer: {
                    Footnote("Loadside keeps its own copy of each installed IPA, so refreshes work after you delete the download. Changing the certificate name creates a new certificate.")
                }
                Section {
                    HStack {
                        TextField("Server", text: $anisette).onSubmit(saveAnisette)
                        if anisette != c.anisetteUrl { Button("Save", action: saveAnisette) }
                    }
                    Toggle("Sign in as a current Mac", isOn: Binding(
                        get: { c.modernClientIdentity ?? true },
                        set: { v in Task { await store.setConfig(["modern_client_identity": .bool(v)]) } }))
                    HStack {
                        Button("Use Default Server") {
                            anisette = "https://ani.iloader.app"
                            Task { await store.setConfig(["anisette_url": .string(anisette)]) }
                        }
                        Button("Reset Machine Identity…") { confirmReset = true }
                    }
                } header: {
                    Text("Anisette")
                } footer: {
                    Footnote("Provides the machine identity Apple needs at sign-in; it never sees your credentials. Leave \"current Mac\" on unless you run your own up-to-date server.")
                }
            }
            Section("Background service") {
                LabeledContent("Status", value: store.connection == .connected ? "Running · v\(store.version)" : "Not running")
                LabeledContent("Memory", value: memory)
                    .task(id: store.pid) { memory = await Self.readMemory(pid: store.pid) }
                HStack {
                    Button("Restart") { Task { await store.restartDaemon() } }
                    Button("Show Logs") { openWindow(id: "logs") }
                    Button("Open Data Folder") { NSWorkspace.shared.open(Store.dataDir) }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear {
            anisette = store.config?.anisetteUrl ?? ""
            machineName = store.config?.machineName ?? ""
        }
        // Pick up changes made elsewhere (CLI) or the daemon's normalized value.
        .onChange(of: store.config?.anisetteUrl) { _, v in anisette = v ?? anisette }
        .onChange(of: store.config?.machineName) { _, v in machineName = v ?? machineName }
        .confirmationDialog("Reset machine identity?", isPresented: $confirmReset) {
            Button("Reset", role: .destructive) { Task { await store.resetAnisette() } }
        } message: {
            Text("Fixes most persistent sign-in errors. Apple may ask for a 2FA code on the next sign-in.")
        }
    }

    /// Daemon RSS, read off the main thread.
    private static func readMemory(pid: Int) async -> String {
        await Task.detached {
            guard pid > 0 else { return "—" }
            var info = rusage_info_current()
            let r = withUnsafeMutablePointer(to: &info) {
                $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) { proc_pid_rusage(Int32(pid), RUSAGE_INFO_CURRENT, $0) }
            }
            guard r == 0 else { return "—" }
            return String(format: "%.1f MB", Double(info.ri_phys_footprint) / 1_048_576)
        }.value
    }

    private func saveMachineName() {
        let v = machineName.trimmingCharacters(in: .whitespaces)
        guard !v.isEmpty else { return }
        Task { await store.setConfig(["machine_name": .string(v)]) }
    }

    private func saveAnisette() {
        Task { await store.setConfig(["anisette_url": .string(anisette.trimmingCharacters(in: .whitespaces))]) }
    }
}

/// Show the main window (creating it if it was closed).
@MainActor func bringMainWindowForward() {
    AppDelegate.startHidden = false
    NSApp.setActivationPolicy(.regular)
    // Hidden (⌘H) windows report not visible; unhide rather than open a second one.
    if NSApp.isHidden { NSApp.unhide(nil) }
    if let w = NSApp.windows.first(where: { $0.identifier?.rawValue.hasPrefix("main") == true && ($0.isVisible || $0.isMiniaturized) }) {
        if w.isMiniaturized { w.deminiaturize(nil) }
        w.makeKeyAndOrderFront(nil)
    } else {
        // SwiftUI disposes of a closed window; ask it for a new one.
        AppDelegate.openMainWindow?()
    }
    NSApp.activate(ignoringOtherApps: true)
}
