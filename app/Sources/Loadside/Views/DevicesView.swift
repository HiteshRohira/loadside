import SwiftUI

struct DevicesView: View {
    @Environment(Store.self) private var store
    @State private var refreshing = false

    var body: some View {
        Group {
            if store.devices.isEmpty {
                // Same placement as the other empty states (centred in the pane).
                ContentUnavailableView {
                    Label("No devices", systemImage: "iphone.slash")
                } description: {
                    Text("Connect an iPhone or iPad with a cable, unlock it and tap \"Trust\".\nWith Wi-Fi sync on, it also appears here when it's on the same Wi-Fi as this Mac.")
                }
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        ForEach(store.devices) { d in DeviceCard(device: d) }
                    }
                    .padding(20)
                    .frame(maxWidth: 760, alignment: .leading)
                    .frame(maxWidth: .infinity)
                }
            }
        }
        .navigationTitle("Devices")
        .toolbar {
            ToolbarItem {
                Button {
                    Task { refreshing = true; await store.refreshDevices(); refreshing = false }
                } label: { Label("Rescan", systemImage: "arrow.clockwise") }
                    .disabled(refreshing)
            }
        }
    }
}

struct DeviceCard: View {
    @Environment(Store.self) private var store
    let device: DeviceInfo

    var body: some View {
        Card {
            HStack(alignment: .top, spacing: 14) {
                Image(systemName: device.model?.hasPrefix("iPad") == true ? "ipad" : "iphone")
                    .font(.system(size: 34, weight: .light)).frame(width: 44)
                VStack(alignment: .leading, spacing: 4) {
                    Text(device.name).font(.headline)
                    Text([device.model, device.iosVersion.map { "iOS \($0)" }].compactMap { $0 }.joined(separator: " · "))
                        .font(.callout).foregroundStyle(.secondary)
                    Text(device.udid).font(.caption2.monospaced()).foregroundStyle(.tertiary).textSelection(.enabled)
                }
                Spacer()
                Label(device.connectionLabel, systemImage: device.connectionIcon)
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let p = device.problem {
                Label(p, systemImage: "exclamationmark.triangle").font(.callout).foregroundStyle(.orange)
            }
            switch device.developerMode {
            case .some(false):
                VStack(alignment: .leading, spacing: 6) {
                    Label("Developer Mode is off. Sideloaded apps won't open until you turn it on: Settings → Privacy & Security → Developer Mode, then restart.",
                          systemImage: "hammer").font(.callout).foregroundStyle(.orange)
                    if device.isUSB {
                        Button("Show the Developer Mode switch on the device") { Task { await store.revealDeveloperMode(device.udid) } }
                            .controlSize(.small)
                    }
                }
            case .some(true):
                Label("Developer Mode on", systemImage: "checkmark.seal").font(.caption).foregroundStyle(.green)
            case .none:
                EmptyView()
            }
            Divider()
            WifiSyncSection(device: device)
            let apps = store.apps.filter { $0.deviceUdid == device.udid }
            if !apps.isEmpty {
                Divider()
                Text("\(apps.count) app\(apps.count == 1 ? "" : "s") from Loadside: " + apps.map(\.name).joined(separator: ", "))
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}


/// "Show this iPhone when on Wi-Fi" for one device.
struct WifiSyncSection: View {
    @Environment(Store.self) private var store
    let device: DeviceInfo
    @State private var busy = false

    var body: some View {
        if device.connection == "wifi" {
            Label("Connected over Wi-Fi", systemImage: "wifi").font(.callout).foregroundStyle(.green)
        } else {
            switch device.wifiSync {
            case .some(true):
                Label("Wi-Fi sync on: also works without the cable on the same Wi-Fi network", systemImage: "wifi")
                    .font(.callout).foregroundStyle(.secondary)
            case .some(false):
                HStack {
                    Text("Wi-Fi sync is off.").font(.callout).foregroundStyle(.secondary)
                    Button {
                        Task { busy = true; await store.enableWifiSync(device.udid); busy = false }
                    } label: {
                        HStack(spacing: 5) {
                            if busy { ProgressView().controlSize(.small) }
                            Text("Turn On Wi-Fi Sync")
                        }
                    }
                    .disabled(busy || !device.isUSB)
                }
            case .none:
                EmptyView()
            }
        }
    }
}
