import SwiftUI

struct SettingsView: View {
    @Environment(Store.self) private var store
    @State private var anisette = ""
    @State private var machineName = ""
    @State private var confirmReset = false
    @State private var memory = "—"
    @AppStorage(Prefs.menuBar) private var showMenuBar = true

    var body: some View {
        Form {
            if let c = store.config {
                Section("Automatic refresh") {
                    Toggle("Refresh apps before they expire", isOn: bind(\.autoRefresh, "auto_refresh"))
                    LabeledContent("Refresh when less than") {
                        Stepper("\(c.refreshThresholdHours) hours left",
                                value: Binding(get: { c.refreshThresholdHours }, set: { v in Task { await store.setConfig(["refresh_threshold_hours": .number(Double(v))]) } }),
                                in: 6...144, step: 6)
                    }
                    Picker("Check every", selection: Binding(get: { c.checkIntervalMinutes }, set: { v in Task { await store.setConfig(["check_interval_minutes": .number(Double(v))]) } })) {
                        ForEach([15, 30, 60, 120, 240], id: \.self) { Text("\($0) minutes").tag($0) }
                    }
                    Text("Loadside also checks the moment a device connects over USB or Wi-Fi.").font(.caption).foregroundStyle(.secondary)
                    Toggle("Show notifications", isOn: bind(\.notifications, "notifications"))
                    Toggle("During automatic refresh, revoke old Loadside certificates if the limit is hit", isOn: bind(\.autoRevokeOwnCerts, "auto_revoke_own_certs"))
                    Toggle("Keep a copy of installed IPAs (needed to refresh if you delete the original)", isOn: bind(\.keepLibraryCopy, "keep_library_copy"))
                    Button("Check Now") { Task { await store.checkNow() } }
                }
                Section("Signing") {
                    HStack {
                        TextField("Certificate machine name", text: $machineName)
                            .onSubmit(saveMachineName)
                        if machineName != c.machineName {
                            Button("Save", action: saveMachineName).disabled(machineName.trimmingCharacters(in: .whitespaces).isEmpty)
                        }
                    }
                    Text("Shown on the certificate in your Apple account. Changing it creates a new certificate.").font(.caption).foregroundStyle(.secondary)
                }
                Section("Anisette server") {
                    HStack {
                        TextField("URL", text: $anisette)
                            .onSubmit(saveAnisette)
                        if anisette != c.anisetteUrl {
                            Button("Save", action: saveAnisette)
                        }
                    }
                    HStack {
                        Button("Use Default") {
                            anisette = "https://ani.iloader.app"
                            Task { await store.setConfig(["anisette_url": .string(anisette)]) }
                        }
                        Button("Reset Machine Identity…") { confirmReset = true }
                    }
                    Text("Provides the machine identity Apple requires for sign-in (it never sees your credentials). You can self-host an anisette-v3 server and put its URL here.")
                        .font(.caption).foregroundStyle(.secondary)
                    Toggle("Sign in as a current Mac", isOn: Binding(
                        get: { c.modernClientIdentity ?? true },
                        set: { v in Task { await store.setConfig(["modern_client_identity": .bool(v)]) } }))
                    Text("Public anisette servers identify as macOS 10.12, which Apple now answers with SMS-only verification and errors. Leave this on unless you run your own up-to-date server.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            Section("Menu bar") {
                Toggle("Show Loadside in the menu bar", isOn: $showMenuBar)
                Text("Shows devices, app expiry and quick refresh. Closing the window keeps the icon; turn this off to have Loadside quit when its window closes.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Background service") {
                LabeledContent("Status", value: store.connection == .connected ? "Running (v\(store.version), pid \(store.pid))" : "Not running")
                LabeledContent("Memory", value: memory)
                    .task(id: store.pid) { memory = await Self.readMemory(pid: store.pid) }
                HStack {
                    Button("Restart Service") { Task { await store.restartDaemon() } }
                    Button("Open Data Folder") {
                        let u = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Loadside")
                        NSWorkspace.shared.open(u)
                    }
                }
                Text("The service starts at login, watches for your devices and refreshes apps. This window can be closed; the service keeps working.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Settings")
        .onAppear {
            anisette = store.config?.anisetteUrl ?? ""
            machineName = store.config?.machineName ?? ""
        }
        // Pick up changes made elsewhere (CLI, another window) or the
        // daemon's normalized value after saving.
        .onChange(of: store.config?.anisetteUrl) { _, v in anisette = v ?? anisette }
        .onChange(of: store.config?.machineName) { _, v in machineName = v ?? machineName }
        .confirmationDialog("Reset machine identity?", isPresented: $confirmReset) {
            Button("Reset", role: .destructive) { Task { await store.resetAnisette() } }
        } message: {
            Text("Fixes most persistent sign-in errors. Apple may ask for a 2FA code on the next sign-in.")
        }
    }

    /// Daemon RSS, read off the main thread (never block or spin the run loop in `body`).
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

    private func bind(_ kp: KeyPath<DaemonConfig, Bool>, _ key: String) -> Binding<Bool> {
        Binding(get: { store.config?[keyPath: kp] ?? false }, set: { v in Task { await store.setConfig([key: .bool(v)]) } })
    }
}
