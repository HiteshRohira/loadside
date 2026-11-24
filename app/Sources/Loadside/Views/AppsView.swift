import SwiftUI

/// Everything about one installed app, opened by clicking its card.
struct AppDetailSheet: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    let appId: String
    @State private var confirmRemove = false

    private var app: AppRecord? { store.apps.first { $0.id == appId } }

    var body: some View {
        Group {
            if let app { content(app) } else { Color.clear.frame(width: 1, height: 1).onAppear { dismiss() } }
        }
        .presentsPrompts()
    }

    private func content(_ app: AppRecord) -> some View {
        let job = store.jobs.last { $0.appId == app.id && $0.isActive }
        let connected = store.devices.contains { $0.udid == app.deviceUdid }
        let expiry = Expiry.of(app)
        return VStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 20) {
                HStack(spacing: 16) {
                    AppIconView(path: app.sourcePath, name: app.name, size: 72)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(app.name).font(.title2.weight(.semibold))
                        Text(app.version.map { "Version \($0)" } ?? "Unknown version").font(.callout).foregroundStyle(.secondary)
                        Text(app.bundleId).font(.caption).foregroundStyle(.tertiary).textSelection(.enabled)
                    }
                    Spacer()
                }

                if let job {
                    JobProgressView(job: job)
                } else if app.onDevice == false {
                    Label("This app was deleted from \(app.deviceName). It won't be refreshed until you reinstall it.", systemImage: "trash")
                        .font(.callout).foregroundStyle(.secondary)
                } else {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack(alignment: .firstTextBaseline) {
                            Text(expiry.text).font(.title3.weight(.semibold))
                            Spacer()
                            if let e = app.expiresAt { Text(Fmt.date(e)).font(.callout).foregroundStyle(.secondary) }
                        }
                        ExpiryBar(expiry: expiry)
                    }
                }
                if let e = app.lastError, job == nil { FailureView(failure: e) }

                VStack(spacing: 0) {
                    row("Device") {
                        HStack(spacing: 6) {
                            StatusDot(color: connected ? .green : .secondary)
                            Text(app.deviceName.isEmpty ? "Unknown" : app.deviceName)
                            Text(connected ? "Connected" : "Not connected").foregroundStyle(.secondary)
                        }
                    }
                    Divider()
                    row("Last refreshed") { Text(Fmt.rel(app.lastRefreshedAt ?? app.installedAt)) }
                    Divider()
                    row("Installed") { Text(Fmt.date(app.installedAt)) }
                    Divider()
                    row("Refresh automatically") {
                        Toggle("", isOn: Binding(get: { app.options.autoRefresh }, set: { v in Task { await store.setAutoRefresh(app, v) } }))
                            .labelsHidden().toggleStyle(.switch).controlSize(.small)
                    }
                    if !app.options.inject.isEmpty {
                        Divider()
                        row("Tweaks") { Text(app.options.inject.map { URL(fileURLWithPath: $0).lastPathComponent }.joined(separator: ", ")).lineLimit(1) }
                    }
                }
                .surface(cornerRadius: 14, padding: nil)
            }
            .padding(24)
            Divider()
            HStack {
                Button("Remove…", role: .destructive) { confirmRemove = true }
                Button { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: app.sourcePath)]) } label: {
                    Image(systemName: "folder")
                }
                .help("Show the original IPA in Finder")
                Spacer()
                Button("Close") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(app.onDevice == false ? "Reinstall" : "Refresh Now") { Task { await store.refresh(app) } }
                    .primaryButton()
                    .keyboardShortcut(.defaultAction)
                    .disabled(job != nil || !connected)
                    .help(connected ? "Re-sign and reinstall now. App data is kept." : "Connect \(app.deviceName) first")
            }
            .controlSize(.large)
            .padding(16)
        }
        .frame(width: 480)
        .confirmationDialog("Remove \(app.name)?", isPresented: $confirmRemove) {
            Button("Delete from Device and Loadside", role: .destructive) { Task { await store.remove(app, uninstall: true); dismiss() } }
            Button("Only Stop Refreshing") { Task { await store.remove(app, uninstall: false); dismiss() } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Deleting it from the device frees one of the 3 app slots free accounts get.")
        }
    }

    private func row<V: View>(_ title: String, @ViewBuilder _ value: () -> V) -> some View {
        HStack {
            Text(title).foregroundStyle(.secondary)
            Spacer()
            value()
        }
        .font(.callout)
        .padding(.horizontal, 14).padding(.vertical, 10)
    }
}
