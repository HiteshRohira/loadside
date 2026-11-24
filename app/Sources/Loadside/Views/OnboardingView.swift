import SwiftUI

enum OnboardingStep: Int, CaseIterable, Identifiable {
    case welcome, account, device, developerMode, wifi, done
    var id: Int { rawValue }
    var title: String {
        switch self {
        case .welcome: "Welcome"
        case .account: "Apple ID"
        case .device: "Connect device"
        case .developerMode: "Developer Mode"
        case .wifi: "Wi-Fi sync"
        case .done: "Ready"
        }
    }
    /// Checklist items this step is about.
    var checkIds: [String] {
        switch self {
        case .welcome: ["service", "internet"]
        case .account: ["account"]
        case .device: ["device"]
        case .developerMode: ["developer_mode"]
        case .wifi: ["wifi_sync"]
        case .done: []
        }
    }
}

/// First-run guide. Each step shows the live state of what it needs and the
/// buttons that fix it, so the user never has to guess what's missing.
struct OnboardingView: View {
    @Environment(Store.self) private var store
    @State private var step: OnboardingStep = .welcome

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Divider()
            VStack(spacing: 0) {
                ScrollView {
                    page
                        .padding(28)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                Divider()
                footer
            }
        }
        .frame(width: 820, height: 600)
        .task {
            // Keep the checks live while the guide is open.
            while !Task.isCancelled {
                await store.loadSetup()
                try? await Task.sleep(for: .seconds(4))
            }
        }
        .sheet(item: Binding(get: { store.prompts.first }, set: { _ in })) { prompt in
            PromptSheet(prompt: prompt).environment(store).interactiveDismissDisabled()
        }
    }

    // MARK: Sidebar

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 28, height: 28)
                Text("Set up Loadside").font(.headline)
            }
            .padding(.bottom, 14)
            ForEach(OnboardingStep.allCases) { s in
                Button { step = s } label: {
                    HStack(spacing: 8) {
                        StatusIcon(status: status(of: s), size: 13)
                        Text(s.title).font(.callout)
                        Spacer()
                    }
                    .padding(.vertical, 6).padding(.horizontal, 8)
                    .background(step == s ? Color.accentColor.opacity(0.15) : .clear, in: RoundedRectangle(cornerRadius: 6))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            Spacer()
            Text("Reopen this guide anytime from Help → Setup Guide.")
                .font(.caption2).foregroundStyle(.tertiary)
        }
        .padding(16)
        .frame(width: 210)
        .background(.background.secondary)
    }

    private func status(of s: OnboardingStep) -> CheckStatus {
        guard let r = store.setup else { return .pending }
        if s == .done { return r.ready ? .ok : .pending }
        let checks = s.checkIds.compactMap { r.check($0) }
        if checks.isEmpty { return .pending }
        if checks.contains(where: { $0.status == .error }) { return .error }
        if checks.contains(where: { $0.status == .action }) { return .action }
        if checks.contains(where: { $0.status == .warning }) { return .warning }
        if checks.allSatisfy({ $0.status == .ok }) { return .ok }
        return checks.first?.status ?? .pending
    }

    // MARK: Footer

    private var footer: some View {
        HStack {
            if step != .welcome {
                Button("Back") { move(-1) }
            }
            Spacer()
            if step == .done {
                Button("Start Installing") { Task { await store.finishOnboarding() } }
                    .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
            } else {
                if step == .welcome || status(of: step) == .ok {
                    Button(step == .welcome ? "Get Started" : "Continue") { move(1) }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                } else {
                    Button(step == .wifi ? "Skip" : "Skip for now") { move(1) }
                        .buttonStyle(.bordered)
                }
            }
        }
        .padding(16)
    }

    private func move(_ d: Int) {
        let next = OnboardingStep(rawValue: step.rawValue + d) ?? step
        withAnimation(.easeInOut(duration: 0.15)) { step = next }
    }

    private func checks(_ ids: [String]) -> [SetupCheck] {
        ids.compactMap { store.setup?.check($0) }
    }

    // MARK: Pages

    @ViewBuilder private var page: some View {
        switch step {
        case .welcome: welcome
        case .account: account
        case .device: device
        case .developerMode: developerMode
        case .wifi: wifi
        case .done: done
        }
    }

    private func header(_ title: String, _ subtitle: String, icon: String) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Image(systemName: icon).font(.system(size: 30, weight: .light)).foregroundStyle(.tint).frame(width: 40)
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.title2.weight(.semibold))
                Text(subtitle).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.bottom, 8)
    }

    private var welcome: some View {
        VStack(alignment: .leading, spacing: 18) {
            header("Sideload apps with your Apple ID",
                   "Loadside signs IPA files with your own Apple ID, installs them on your iPhone or iPad, and re-signs them before they expire, all in the background. No Xcode needed.",
                   icon: "square.and.arrow.down.on.square")
            Card {
                Text("What you'll need").font(.headline)
                Label("An Apple ID (a free one works)", systemImage: "person.crop.circle")
                Label("Your iPhone or iPad and a USB cable (only for the first setup)", systemImage: "cable.connector")
                Label("Optional: the device on the same Wi-Fi as this Mac, for refreshes without the cable", systemImage: "wifi")
            }
            Card {
                Text("System check").font(.headline)
                if store.setup == nil { ProgressView().controlSize(.small) }
                ForEach(checks(OnboardingStep.welcome.checkIds)) { c in CheckRow(check: c, compact: true) }
            }
        }
    }

    private var account: some View {
        VStack(alignment: .leading, spacing: 16) {
            header("Sign in with your Apple ID",
                   "Apps are signed with a certificate from your Apple ID. Your password is sent only to Apple and stored in your Keychain so Loadside can refresh apps on its own.",
                   icon: "person.crop.circle.badge.checkmark")
            if store.account == nil {
                LoginCard()
            } else {
                ForEach(checks(["account"])) { c in Card { CheckRow(check: c) } }
            }
            Text("Tip: free accounts can keep 3 sideloaded apps per device, and each app must be re-signed every 7 days. Loadside does that automatically while it can reach the device.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var device: some View {
        VStack(alignment: .leading, spacing: 16) {
            header("Connect your iPhone or iPad",
                   "The cable is needed only the first time, so the device can trust this Mac.",
                   icon: "iphone.gen3.radiowaves.left.and.right")
            CableOnceNote()
            Card {
                Steps(items: [
                    "Connect the device to this Mac with a USB cable.",
                    "Unlock it.",
                    "When it asks **Trust This Computer?**, tap **Trust** and enter your passcode.",
                ])
            }
            ForEach(checks(["device"])) { c in Card { CheckRow(check: c) } }
            if !store.devices.isEmpty {
                Card {
                    ForEach(store.devices) { d in
                        HStack {
                            Image(systemName: d.model?.hasPrefix("iPad") == true ? "ipad" : "iphone")
                            Text(d.name).font(.callout.weight(.medium))
                            Text([d.model, d.iosVersion.map { "iOS \($0)" }].compactMap { $0 }.joined(separator: " · "))
                                .font(.caption).foregroundStyle(.secondary)
                            Spacer()
                            Label(d.connectionLabel, systemImage: d.connectionIcon).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
            }
        }
    }

    private var developerMode: some View {
        VStack(alignment: .leading, spacing: 16) {
            header("Turn on Developer Mode",
                   "iOS 16 and later only open sideloaded apps when Developer Mode is on. It's a one-time switch on the device.",
                   icon: "hammer")
            ForEach(checks(["developer_mode"])) { c in Card { CheckRow(check: c) } }
            if checks(["developer_mode"]).contains(where: { $0.status != .ok }) {
                Card {
                Text("On the device").font(.headline)
                Steps(items: [
                    "Open **Settings → Privacy & Security**.",
                    "Scroll to the bottom and tap **Developer Mode**. If it's missing, click **Show the switch** above while the cable is connected.",
                    "Turn it on and tap **Restart**.",
                    "After the restart, unlock the device and tap **Turn On**.",
                ])
                }
            }
        }
    }

    private var wifi: some View {
        VStack(alignment: .leading, spacing: 16) {
            header("Install without a cable",
                   "Optional. With Wi-Fi sync on, Loadside installs and refreshes apps whenever the device is on the same Wi-Fi network as this Mac.",
                   icon: "wifi")
            ForEach(checks(["wifi_sync"])) { c in Card { CheckRow(check: c) } }
            Text("This is the same setting as Finder's \"Show this iPhone when on Wi-Fi\". It needs the cable once to turn on.")
                .font(.caption).foregroundStyle(.secondary)
            CableOnceNote()
        }
    }

    private var done: some View {
        VStack(alignment: .leading, spacing: 16) {
            header(store.setup?.ready == true ? "You're all set" : "Almost there",
                   store.setup?.ready == true
                       ? "Drop an IPA onto the Loadside window. It stays signed in the background."
                       : "Some essentials still need attention. The main window shows what's left.",
                   icon: store.setup?.ready == true ? "checkmark.seal" : "list.bullet.clipboard")
            Card {
                ForEach(store.setup?.checks ?? []) { c in
                    HStack(spacing: 8) {
                        StatusIcon(status: c.status, size: 13)
                        Text(c.title).font(.callout)
                        Spacer()
                        Text(c.summary).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail).frame(maxWidth: 380, alignment: .trailing)
                    }
                }
            }
            Text("After your first install, if the app says **Untrusted Developer**: Settings → General → VPN & Device Management → your Apple ID → Trust.")
                .font(.callout).foregroundStyle(.secondary)
        }
    }
}

/// Essentials + optional checks, each with the button that fixes it.
struct SetupChecklist: View {
    @Environment(Store.self) private var store

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            if let r = store.setup {
                group(r.checks.filter { $0.group == "essentials" })
                CableOnceNote()
                let optional = r.checks.filter { $0.group == "optional" }
                if !optional.isEmpty {
                    Text("Optional").font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
                    group(optional)
                }
            } else {
                ProgressView("Checking…").frame(maxWidth: .infinity).padding(.vertical, 30)
            }
        }
    }

    private func group(_ checks: [SetupCheck]) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(checks.enumerated()), id: \.element.id) { i, c in
                if i > 0 { Divider() }
                CheckRow(check: c)
            }
        }
        .surface(cornerRadius: 14, padding: 14)
    }
}

/// The checklist as a sheet, opened from the home screen's "Fix" banner.
struct SetupSheet: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var refreshing = false

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                let ready = store.setup?.ready == true
                Image(systemName: ready ? "checkmark.seal.fill" : "list.bullet.clipboard")
                    .font(.system(size: 26)).foregroundStyle(ready ? .green : .orange)
                VStack(alignment: .leading, spacing: 2) {
                    Text(ready ? "Ready to install" : "Finish setting up").font(.title3.weight(.semibold))
                    Text(ready ? "Everything essential is done." : "Each item says what to do.")
                        .font(.callout).foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(20)
            Divider()
            ScrollView { SetupChecklist().padding(20) }
            Divider()
            HStack {
                Button("Setup Guide") { dismiss(); DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { store.reopenOnboarding() } }
                Spacer()
                Button {
                    Task { refreshing = true; await store.loadSetup(force: true); refreshing = false }
                } label: { Text(refreshing ? "Checking…" : "Check Again") }
                    .disabled(refreshing)
                Button("Done") { dismiss() }.keyboardShortcut(.defaultAction).primaryButton()
            }
            .controlSize(.large)
            .padding(16)
        }
        .frame(width: 560, height: 600)
        .presentsPrompts()
        .task {
            while !Task.isCancelled {
                await store.loadSetup()
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }
}
