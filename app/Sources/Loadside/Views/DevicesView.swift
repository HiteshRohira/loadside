import SwiftUI

/// Device facts and the switches that matter (Developer Mode, Wi-Fi sync).
struct DeviceDetails: View {
    @Environment(Store.self) private var store
    let device: DeviceInfo

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: device.model?.hasPrefix("iPad") == true ? "ipad" : "iphone")
                    .font(.system(size: 30, weight: .light)).frame(width: 36)
                VStack(alignment: .leading, spacing: 3) {
                    Text(device.name).font(.headline)
                    Text([device.model, device.iosVersion.map { "iOS \($0)" }, device.connectionLabel].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
            }
            if let p = device.problem {
                Label(p, systemImage: "exclamationmark.triangle.fill").font(.callout).foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            switch device.developerMode {
            case .some(false):
                VStack(alignment: .leading, spacing: 6) {
                    Label("Developer Mode is off", systemImage: "hammer").font(.callout.weight(.medium)).foregroundStyle(.orange)
                    Text("Sideloaded apps won't open until you turn it on: Settings → Privacy & Security → Developer Mode, then restart.")
                        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    if device.isUSB {
                        Button("Show the Switch on the Device") { Task { await store.revealDeveloperMode(device.udid) } }
                            .controlSize(.small)
                    }
                }
            case .some(true):
                Label("Developer Mode on", systemImage: "checkmark.circle.fill").font(.callout).foregroundStyle(.green)
            case .none:
                EmptyView()
            }
            WifiSyncSection(device: device)
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
                Label("Wi-Fi sync on", systemImage: "wifi").font(.callout).foregroundStyle(.green)
                    .help("Works without the cable when it's on the same Wi-Fi as this Mac")
            case .some(false):
                HStack {
                    Label("Wi-Fi sync off", systemImage: "wifi.slash").font(.callout).foregroundStyle(.secondary)
                    Spacer()
                    Button {
                        Task { busy = true; await store.enableWifiSync(device.udid); busy = false }
                    } label: {
                        HStack(spacing: 5) {
                            if busy { ProgressView().controlSize(.small) }
                            Text("Turn On")
                        }
                    }
                    .controlSize(.small)
                    .disabled(busy || !device.isUSB)
                }
            case .none:
                EmptyView()
            }
        }
    }
}
