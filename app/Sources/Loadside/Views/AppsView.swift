import SwiftUI

struct AppsView: View {
    @Environment(Store.self) private var store
    @State private var removing: AppRecord?

    var body: some View {
        Group {
            if store.apps.isEmpty {
                ContentUnavailableView("No apps yet", systemImage: "square.grid.2x2",
                                       description: Text("Apps you install with Loadside show up here with their expiry dates."))
            } else {
                List {
                    Section {
                        ForEach(store.apps.sorted { ($0.expiresAt ?? .max) < ($1.expiresAt ?? .max) }) { app in
                            AppRow(app: app, job: store.jobs.last { $0.appId == app.id && $0.isActive }, onRemove: { removing = app })
                        }
                    } header: {
                        summary
                    }
                }
                .listStyle(.inset)
            }
        }
        .navigationTitle("Apps")
        .toolbar {
            ToolbarItem {
                Button { Task { await store.refreshAll() } } label: { Label("Refresh All", systemImage: "arrow.clockwise") }
                    .help("Re-sign and reinstall every app on connected devices")
                    .disabled(store.apps.isEmpty)
            }
        }
        .confirmationDialog("Remove \(removing?.name ?? "")?", isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }), presenting: removing) { app in
            Button("Delete from Device and Loadside", role: .destructive) { Task { await store.remove(app, uninstall: true) } }
            Button("Only Stop Tracking") { Task { await store.remove(app, uninstall: false) } }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Deleting from the device frees one of the 3 app slots free accounts get.")
        }
    }

    private var summary: some View {
        HStack {
            if store.config?.autoRefresh == true {
                Label("Auto-refresh on", systemImage: "arrow.triangle.2.circlepath").foregroundStyle(.green)
                if let t = store.nextCheckAt { Text("· next check \(Fmt.rel(t))").foregroundStyle(.secondary) }
            } else {
                Label("Auto-refresh off (Settings)", systemImage: "pause.circle").foregroundStyle(.orange)
            }
            Spacer()
            Text("\(store.apps.count) app\(store.apps.count == 1 ? "" : "s")").foregroundStyle(.secondary)
        }
        .font(.caption)
    }
}

struct AppRow: View {
    @Environment(Store.self) private var store
    let app: AppRecord
    let job: JobInfo?
    let onRemove: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            RoundedRectangle(cornerRadius: 10).fill(.tint.opacity(0.15)).frame(width: 40, height: 40)
                .overlay(Text(String(app.name.prefix(1))).font(.headline).foregroundStyle(.tint))
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(app.name).font(.headline)
                    if let v = app.version { Text(v).font(.caption).foregroundStyle(.secondary) }
                }
                HStack(spacing: 6) {
                    ExpiryBadge(expiresAt: app.expiresAt)
                    Label(app.deviceName.isEmpty ? "Device" : app.deviceName, systemImage: deviceConnected ? "iphone" : "iphone.slash")
                        .font(.caption).foregroundStyle(deviceConnected ? .secondary : .tertiary)
                    if !app.options.autoRefresh { Tag(text: "Auto-refresh off", color: .orange) }
                    if app.onDevice == false { Tag(text: "Not on device", color: .red) }
                    if !app.options.inject.isEmpty { Tag(text: "\(app.options.inject.count) tweak\(app.options.inject.count == 1 ? "" : "s")") }
                }
                Text(app.bundleId).font(.caption2).foregroundStyle(.tertiary).textSelection(.enabled)
                if let job {
                    JobProgressView(job: job).padding(.top, 4)
                } else if let e = app.lastError {
                    FailureView(failure: e).padding(.top, 4)
                }
            }
            Spacer()
            HStack {
                Button(app.onDevice == false ? "Reinstall" : "Refresh") { Task { await store.refresh(app) } }
                    .disabled(job != nil || !deviceConnected)
                    .help(deviceConnected ? "Re-sign and reinstall now (keeps app data)" : "Connect \(app.deviceName) first")
                Menu {
                    Toggle("Auto-refresh", isOn: Binding(get: { app.options.autoRefresh }, set: { v in Task { await store.setAutoRefresh(app, v) } }))
                    Button("Show Original IPA in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: app.sourcePath)]) }
                    Divider()
                    Button("Remove…", role: .destructive, action: onRemove)
                } label: { Image(systemName: "ellipsis.circle") }
                    .menuStyle(.borderlessButton).fixedSize()
            }
        }
        .padding(.vertical, 6)
    }

    private var deviceConnected: Bool { store.devices.contains { $0.udid == app.deviceUdid } }
}
