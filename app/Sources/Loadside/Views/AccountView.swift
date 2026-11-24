import SwiftUI

struct AccountView: View {
    @Environment(Store.self) private var store

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if let a = store.account {
                    accountCard(a)
                    AppIdsCard()
                    CertsCard()
                } else {
                    LoginCard()
                }
            }
            .padding(20)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity)
        }
        .navigationTitle("Account")
        .task(id: store.account?.email) { await store.loadAccountDetails() }
    }

    private func accountCard(_ a: AccountInfo) -> some View {
        Card {
            HStack(alignment: .top) {
                Image(systemName: "person.crop.circle.fill").font(.system(size: 40)).foregroundStyle(.tint)
                VStack(alignment: .leading, spacing: 3) {
                    Text(a.name ?? a.email).font(.title3.weight(.semibold))
                    Text(a.email).font(.callout).foregroundStyle(.secondary)
                    HStack(spacing: 6) {
                        // A personal team is named after the person; only show
                        // the team when it says something new.
                        if let t = a.teamName, t != a.name { Tag(text: "Team: \(t)") }
                        Tag(text: a.isFree == false ? "Paid developer account" : "Free account", color: a.isFree == false ? .green : .blue)
                        if !store.loggedIn { Tag(text: "Session renews on next use", color: .secondary) }
                    }
                }
                Spacer()
                Button("Sign Out") { Task { await store.logout() } }
            }
            if let teams = store.teams, teams.teams.count > 1 {
                Picker("Team", selection: Binding(get: { teams.selected }, set: { id in Task { await store.selectTeam(id) } })) {
                    ForEach(teams.teams) { t in Text("\(t.name ?? t.teamId)\(t.isFree ? " (free)" : "")").tag(t.teamId) }
                }
                .frame(maxWidth: 360)
            }
            if a.isFree != false {
                Text("Free accounts: apps last 7 days, at most 3 sideloaded apps per device, 10 new App IDs per 7 days.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}

struct LoginCard: View {
    @Environment(Store.self) private var store
    @State private var email = ""
    @State private var password = ""
    @State private var remember = true
    @State private var busy = false
    @State private var error: Failure?

    var body: some View {
        Card {
            Text("Sign in with your Apple ID").font(.title3.weight(.semibold))
            Text("A free Apple ID works. Consider a secondary Apple ID just for sideloading.")
                .font(.callout).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 10) {
                TextField("Apple ID email", text: $email)
                    .textContentType(.username).textFieldStyle(.roundedBorder)
                SecureField("Password", text: $password)
                    .textContentType(.password).textFieldStyle(.roundedBorder)
                    .onSubmit { Task { await login() } }
                Toggle("Remember password in Keychain (needed for automatic refresh)", isOn: $remember)
            }
            .frame(maxWidth: 380)
            if let error { FailureView(failure: error) }
            HStack {
                Button {
                    Task { await login() }
                } label: {
                    if busy { ProgressView().controlSize(.small) } else { Text("Sign In") }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(busy || email.isEmpty || password.isEmpty)
                if busy { Text("Talking to Apple… a 2FA code may be requested.").font(.caption).foregroundStyle(.secondary) }
            }
            Divider()
            Label {
                Text("Your password and 2FA codes go only to Apple. Loadside also contacts an anisette server (Settings) that provides a machine identity; it never sees your credentials.")
            } icon: { Image(systemName: "lock.shield") }
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func login() async {
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

struct AppIdsCard: View {
    @Environment(Store.self) private var store
    var body: some View {
        Card {
            HStack {
                Text("App IDs").font(.headline)
                Spacer()
                Button { Task { await store.loadAppIds() } } label: { Image(systemName: "arrow.clockwise") }.buttonStyle(.borderless)
            }
            if let list = store.appIds {
                // The weekly App ID limit only applies to free accounts.
                if store.account?.isFree != false, let avail = list.availableQuantity, let max = list.maxQuantity, max > 0 {
                    Gauge(value: Double(max - avail), in: 0...Double(max)) {
                        Text("\(avail) of \(max) available")
                    }
                    .gaugeStyle(.accessoryLinearCapacity)
                    .tint(avail == 0 ? .red : avail < 3 ? .orange : .green)
                    Text("Each new app (and each of its extensions) uses one. They free up 7 days after creation.")
                        .font(.caption).foregroundStyle(.secondary)
                } else if store.account?.isFree == false {
                    Text("\(list.appIds.count) on your team. Paid accounts have no weekly limit.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                ForEach(list.appIds.sorted { ($0.expiresAt ?? .max) < ($1.expiresAt ?? .max) }) { a in
                    HStack {
                        VStack(alignment: .leading) {
                            Text(a.name).font(.callout)
                            Text(a.identifier).font(.caption2).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Spacer()
                        if store.account?.isFree != false, let e = a.expiresAt { Text("frees \(Fmt.rel(e))").font(.caption).foregroundStyle(.secondary) }
                        if store.account?.isFree == false {
                            Button(role: .destructive) { Task { await store.deleteAppId(a.appIdId) } } label: { Image(systemName: "trash") }
                                .buttonStyle(.borderless)
                        }
                    }
                }
            } else {
                ProgressView().controlSize(.small)
            }
        }
    }
}

struct CertsCard: View {
    @Environment(Store.self) private var store
    @State private var revoking: CertSummary?
    var body: some View {
        Card {
            HStack {
                Text("Certificates").font(.headline)
                Spacer()
                Button { Task { await store.loadCerts() } } label: { Image(systemName: "arrow.clockwise") }.buttonStyle(.borderless)
            }
            Text("Loadside creates one development certificate and reuses it. Revoking a certificate stops apps signed with it (including by other tools) from launching.")
                .font(.caption).foregroundStyle(.secondary)
            if let certs = store.certs {
                if certs.isEmpty { Text("None yet — one is created on your first install.").font(.callout).foregroundStyle(.secondary) }
                ForEach(certs) { c in
                    HStack {
                        VStack(alignment: .leading) {
                            HStack {
                                Text(c.machineName ?? "Unknown machine").font(.callout)
                                if c.ours { Tag(text: "Loadside", color: .blue) }
                            }
                            Text(c.serialNumber).font(.caption2.monospaced()).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Spacer()
                        if let e = c.expiresAt { Text("expires \(Fmt.rel(e))").font(.caption).foregroundStyle(.secondary) }
                        Button("Revoke…", role: .destructive) { revoking = c }.controlSize(.small)
                    }
                }
            } else {
                ProgressView().controlSize(.small)
            }
        }
        .confirmationDialog("Revoke certificate from \(revoking?.machineName ?? "unknown")?", isPresented: Binding(get: { revoking != nil }, set: { if !$0 { revoking = nil } }), presenting: revoking) { c in
            Button("Revoke", role: .destructive) { Task { await store.revokeCert(c.serialNumber) } }
        } message: { _ in Text("Apps signed with it will stop opening until they're re-signed.") }
    }
}
