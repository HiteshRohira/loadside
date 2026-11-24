import SwiftUI

/// Email + password + Sign In. Used on the home screen, in the setup guide and in Settings.
struct LoginForm: View {
    @Environment(Store.self) private var store
    @State private var email = ""
    @State private var password = ""
    @State private var remember = true
    @State private var busy = false
    @State private var error: Failure?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(spacing: 8) {
                TextField("Apple ID email", text: $email)
                    .textContentType(.username)
                SecureField("Password", text: $password)
                    .textContentType(.password)
                    .onSubmit { Task { await login() } }
            }
            .textFieldStyle(.roundedBorder)
            .controlSize(.large)
            Toggle("Remember password (needed for automatic refresh)", isOn: $remember)
                .font(.callout)
            if let error { FailureView(failure: error) }
            Button {
                Task { await login() }
            } label: {
                HStack {
                    if busy { ProgressView().controlSize(.small) }
                    Text(busy ? "Signing in…" : "Sign In")
                }
                .frame(maxWidth: .infinity)
            }
            .primaryButton()
            .controlSize(.large)
            .keyboardShortcut(.defaultAction)
            .disabled(busy || email.isEmpty || password.isEmpty)
            Text(busy ? "Apple may send a code to your other devices." : "Your password goes only to Apple and your Keychain.")
                .font(.caption).foregroundStyle(.secondary)
                .frame(maxWidth: .infinity)
                .help("Loadside also contacts an anisette server (Settings → Advanced) for a machine identity. It never sees your credentials.")
        }
    }

    private func login() async {
        guard !busy else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            try await store.login(email: email, password: password, remember: remember)
            password = ""
        } catch {
            self.error = (error as? Failure) ?? Failure.local(error.localizedDescription)
        }
    }
}

/// Sign-in in a card (setup guide).
struct LoginCard: View {
    var body: some View {
        Card {
            Text("Sign in with your Apple ID").font(.headline)
            Text("A free Apple ID works. A secondary one just for sideloading is a good idea.")
                .font(.callout).foregroundStyle(.secondary)
            LoginForm().frame(maxWidth: 380)
        }
    }
}

/// Settings → Account.
struct AccountSettings: View {
    @Environment(Store.self) private var store
    @State private var revoking: CertSummary?

    var body: some View {
        Group {
            if let a = store.account {
                Form {
                    Section {
                        HStack(spacing: 12) {
                            Image(systemName: "person.crop.circle.fill").font(.system(size: 36)).foregroundStyle(.tint)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(a.name ?? a.email).font(.headline)
                                Text(a.email).font(.callout).foregroundStyle(.secondary)
                            }
                            Spacer()
                            Tag(text: a.isFree == false ? "Paid" : "Free", color: a.isFree == false ? .green : .blue)
                            Button("Sign Out") { Task { await store.logout() } }
                        }
                        if let teams = store.teams, teams.teams.count > 1 {
                            Picker("Team", selection: Binding(get: { teams.selected }, set: { id in Task { await store.selectTeam(id) } })) {
                                ForEach(teams.teams) { t in Text("\(t.name ?? t.teamId)\(t.isFree ? " (free)" : "")").tag(t.teamId) }
                            }
                        }
                    } footer: {
                        if a.isFree != false {
                            Footnote("Free accounts: apps last 7 days, 3 sideloaded apps per device, 10 new App IDs per 7 days.")
                        }
                    }
                    appIds
                    certs
                }
                .formStyle(.grouped)
            } else {
                ScrollView {
                    VStack(spacing: 14) {
                        Text("Sign in with your Apple ID").font(.title3.weight(.semibold))
                        LoginForm().frame(width: 340)
                    }
                    .padding(30)
                    .frame(maxWidth: .infinity)
                }
            }
        }
        .task(id: store.account?.email) { await store.loadAccountDetails() }
        .confirmationDialog("Revoke certificate from \(revoking?.machineName ?? "unknown")?", isPresented: Binding(get: { revoking != nil }, set: { if !$0 { revoking = nil } }), presenting: revoking) { c in
            Button("Revoke", role: .destructive) { Task { await store.revokeCert(c.serialNumber) } }
        } message: { _ in Text("Apps signed with it will stop opening until they're re-signed.") }
    }

    @ViewBuilder private var appIds: some View {
        Section {
            if let list = store.appIds {
                if store.account?.isFree != false, let avail = list.availableQuantity, let max = list.maxQuantity, max > 0 {
                    Gauge(value: Double(max - avail), in: 0...Double(max)) {
                        Text("\(avail) of \(max) left this week")
                    }
                    .gaugeStyle(.accessoryLinearCapacity)
                    .tint(avail == 0 ? .red : avail < 3 ? .orange : .green)
                }
                ForEach(store.appIdGroups) { g in
                    appIdRow(g.main, title: g.main.name, subtitle: g.tracked != nil ? "Refreshed by Loadside" : store.account?.isFree == true ? "Not refreshed by Loadside" : g.main.identifier, isExtension: false)
                    ForEach(g.extensions) { e in
                        appIdRow(e, title: ExtensionName.friendly(e.name), subtitle: "Part of \(g.main.name), uses its own App ID", isExtension: true)
                    }
                }
            } else {
                ProgressView().controlSize(.small)
            }
        } header: {
            HStack {
                Text("App IDs")
                Spacer()
                Button { Task { await store.loadAppIds() } } label: { Image(systemName: "arrow.clockwise") }.buttonStyle(.borderless)
            }
        } footer: {
            if store.account?.isFree != false {
                Footnote("Every new app uses one, and so does each of its widgets and other extensions. Refreshing an app you already have uses none. They free up 7 days after they're created.")
            }
        }
    }

    private func appIdRow(_ a: AppIdSummary, title: String, subtitle: String, isExtension: Bool) -> some View {
        HStack(spacing: 8) {
            if isExtension {
                Image(systemName: "arrow.turn.down.right").font(.caption).foregroundStyle(.tertiary).padding(.leading, 6)
            }
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                Text(subtitle).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if store.account?.isFree != false, let e = a.expiresAt { Text("frees \(Fmt.rel(e))").font(.caption).foregroundStyle(.secondary) }
            if store.account?.isFree == false {
                Button(role: .destructive) { Task { await store.deleteAppId(a.appIdId) } } label: { Image(systemName: "trash") }
                    .buttonStyle(.borderless)
            }
        }
        .help(a.identifier)
        .contextMenu {
            Button("Copy Identifier") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(a.identifier, forType: .string)
            }
        }
    }

    @ViewBuilder private var certs: some View {
        Section {
            if let certs = store.certs {
                if certs.isEmpty { Text("None yet. One is created on your first install.").foregroundStyle(.secondary) }
                ForEach(certs) { c in
                    HStack {
                        VStack(alignment: .leading) {
                            HStack {
                                Text(c.machineName ?? "Unknown machine")
                                if c.ours { Tag(text: "Loadside", color: .blue) }
                            }
                            Text(c.serialNumber).font(.caption.monospaced()).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Spacer()
                        if let e = c.expiresAt { Text("expires \(Fmt.rel(e))").font(.caption).foregroundStyle(.secondary) }
                        Button("Revoke…", role: .destructive) { revoking = c }.controlSize(.small)
                    }
                }
            } else {
                ProgressView().controlSize(.small)
            }
        } header: {
            HStack {
                Text("Certificates")
                Spacer()
                Button { Task { await store.loadCerts() } } label: { Image(systemName: "arrow.clockwise") }.buttonStyle(.borderless)
            }
        } footer: {
            Footnote("Loadside creates one certificate and reuses it. Revoking one stops apps signed with it (by any tool) from opening.")
        }
    }
}
