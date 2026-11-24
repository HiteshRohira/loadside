import SwiftUI

/// The menu bar panel: a glance at the service, devices and app expiry, plus
/// the few actions worth having without opening the window.
struct MenuBarView: View {
    @Environment(Store.self) private var store
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if store.connection == .connected {
                if let blocking = blockingCount, blocking > 0 {
                    row {
                        Button { open("setup") } label: {
                            Label("\(blocking) setup step\(blocking == 1 ? "" : "s") left", systemImage: "exclamationmark.circle.fill")
                                .foregroundStyle(.orange)
                        }.buttonStyle(.plain)
                    }
                    Divider()
                }
                jobsSection
                section("Devices") {
                    if store.devices.isEmpty {
                        Text("No iPhone or iPad connected").font(.callout).foregroundStyle(.secondary)
                    }
                    ForEach(store.devices) { d in
                        HStack(spacing: 8) {
                            Image(systemName: d.connectionIcon).frame(width: 16).foregroundStyle(.secondary)
                            Text(d.name).lineLimit(1)
                            Spacer()
                            Text(d.problem == nil ? d.connectionLabel : "Needs attention")
                                .font(.caption).foregroundStyle(d.problem == nil ? Color.secondary : Color.orange)
                        }
                    }
                }
                Divider()
                section("Apps") {
                    if store.apps.isEmpty {
                        Text("Nothing installed yet").font(.callout).foregroundStyle(.secondary)
                    }
                    ForEach(sortedApps.prefix(6)) { app in
                        HStack(spacing: 8) {
                            Text(app.name).lineLimit(1)
                            Spacer()
                            ExpiryBadge(expiresAt: app.expiresAt)
                            Button { Task { await store.refresh(app) } } label: {
                                Image(systemName: "arrow.clockwise")
                            }
                            .buttonStyle(.borderless)
                            .help("Refresh \(app.name)")
                            .disabled(store.activeJobs.contains { $0.appId == app.id })
                        }
                    }
                    if store.apps.count > 6 {
                        Button("Show all \(store.apps.count) apps…") { open("apps") }.buttonStyle(.link).font(.callout)
                    }
                }
                Divider()
            }
            VStack(alignment: .leading, spacing: 2) {
                if store.connection == .connected && !store.apps.isEmpty {
                    MenuButton("Refresh All Apps", icon: "arrow.clockwise") { Task { await store.refreshAll() } }
                }
                MenuButton("Install an App…", icon: "square.and.arrow.down.on.square") { open("install") }
                MenuButton("Open Loadside", icon: "macwindow") { open(nil) }
                Divider().padding(.vertical, 4)
                MenuButton("Quit Loadside", icon: "power") { NSApp.terminate(nil) }
            }
            .padding(6)
        }
        .frame(width: 320)
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 32, height: 32)
            VStack(alignment: .leading, spacing: 1) {
                Text("Loadside").font(.headline)
                Text(statusLine).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            Circle().fill(statusColor).frame(width: 8, height: 8)
        }
        .padding(12)
    }

    @ViewBuilder private var jobsSection: some View {
        if !store.activeJobs.isEmpty {
            section("Working") {
                ForEach(store.activeJobs) { job in
                    VStack(alignment: .leading, spacing: 3) {
                        Text(job.title).font(.callout).lineLimit(1)
                        ProgressView(value: job.status == .queued ? 0 : job.progress).controlSize(.small)
                        Text(job.stage).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
            }
            Divider()
        }
    }

    private var sortedApps: [AppRecord] {
        store.apps.sorted { ($0.expiresAt ?? .max) < ($1.expiresAt ?? .max) }
    }

    private var blockingCount: Int? {
        store.setup?.checks.filter(\.isBlocking).count
    }

    private var statusLine: String {
        switch store.connection {
        case .connecting: return "Connecting to the service…"
        case .unavailable: return "Background service isn't running"
        case .connected:
            if let n = store.nextCheckAt, store.config?.autoRefresh == true { return "Next check \(Fmt.rel(n))" }
            return store.loggedIn ? "Signed in" : "Not signed in"
        }
    }

    private var statusColor: Color {
        switch store.connection {
        case .connecting: return .yellow
        case .unavailable: return .red
        case .connected: return (blockingCount ?? 0) > 0 ? .orange : .green
        }
    }

    private func section<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title.uppercased()).font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
            content()
        }
        .padding(.horizontal, 12).padding(.vertical, 10)
    }

    private func row<C: View>(@ViewBuilder _ content: () -> C) -> some View {
        content().padding(.horizontal, 12).padding(.vertical, 8)
    }

    private func open(_ target: String?) {
        NSApp.setActivationPolicy(.regular)
        if !NSApp.windows.contains(where: { $0.identifier?.rawValue.hasPrefix("main") == true && $0.isVisible }) {
            openWindow(id: "main")
        }
        NSApp.activate(ignoringOtherApps: true)
        if let target { store.navigate = target }
    }
}

private struct MenuButton: View {
    let title: String
    let icon: String
    let action: () -> Void
    @State private var hover = false

    init(_ title: String, icon: String, action: @escaping () -> Void) {
        self.title = title
        self.icon = icon
        self.action = action
    }

    var body: some View {
        Button(action: action) {
            Label(title, systemImage: icon)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 6).padding(.vertical, 4)
                .background(hover ? Color.accentColor.opacity(0.18) : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
    }
}

/// The menu bar icon reflects whether anything needs attention.
struct MenuBarLabel: View {
    @Environment(Store.self) private var store
    var body: some View {
        let busy = !store.activeJobs.isEmpty
        let attention = store.connection != .connected || (store.setup?.checks.contains(where: \.isBlocking) ?? false)
            || store.apps.contains { ($0.expiresAt ?? .max) < Int(Date().timeIntervalSince1970) + 24 * 3600 }
        Image(systemName: busy ? "arrow.triangle.2.circlepath" : attention ? "iphone.gen3.badge.exclamationmark" : "apps.iphone")
    }
}
