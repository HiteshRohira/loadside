import SwiftUI

/// Everything about one installed app, opened by clicking its card.
struct AppDetailSheet: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    let appId: String
    @State private var confirmRemove = false
    @State private var showNotes = false
    @State private var checkingUpdates = false
    @State private var editingGitHub = false
    @State private var repoInput = ""
    @State private var assetPatternInput = ""
    @State private var policyInput: UpdatePolicy = .onRefresh
    @State private var prereleasesInput = false
    @State private var inspectedAssets: [ReleaseAsset] = []
    @State private var suggestedPatterns: [String: String] = [:]
    @State private var showAssetFilter = false
    @State private var lookingUpRepo = false
    @State private var savingGitHub = false
    @State private var confirmTrustRepo = false
    @State private var repoError: Failure?

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
        let avail = app.onDevice != false ? app.github?.availableRelease : nil
        return VStack(spacing: 0) {
            FittingScrollView(maxHeight: 620) {
                VStack(alignment: .leading, spacing: 14) {
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

                    if let avail, job == nil {
                        updateAvailableBanner(app: app, release: avail, connected: connected)
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

                    githubSection(app: app, job: job)
                }
                .padding(20)
            }
            Divider()
            HStack {
                Button("Remove…", role: .destructive) { confirmRemove = true }
                Button { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: app.sourcePath)]) } label: {
                    Image(systemName: "folder")
                }
                .help("Show the original IPA in Finder")
                Spacer()
                Button("Close") { dismiss() }.keyboardShortcut(.cancelAction)
                if let avail {
                    Button("Refresh") { Task { await store.refresh(app, skipUpdate: true) } }
                        .disabled(job != nil || !connected)
                        .help(connected ? "Re-sign without changing version" : "Connect \(app.deviceName) first")
                    Button("Update to \(avail.tag)") { Task { await store.updateRelease(app) } }
                        .primaryButton()
                        .keyboardShortcut(.defaultAction)
                        .disabled(job != nil || !connected)
                        .help(connected ? "Download \(avail.tag) from GitHub and update on \(app.deviceName). App data is kept." : "Connect \(app.deviceName) first")
                } else {
                    Button(app.onDevice == false ? "Reinstall" : "Refresh Now") { Task { await store.refresh(app) } }
                        .primaryButton()
                        .keyboardShortcut(.defaultAction)
                        .disabled(job != nil || !connected)
                        .help(connected ? "Re-sign and reinstall now. App data is kept." : "Connect \(app.deviceName) first")
                }
            }
            .controlSize(.large)
            .padding(16)
        }
        .frame(width: 510)
        .confirmationDialog("Remove \(app.name)?", isPresented: $confirmRemove) {
            Button("Delete from Device and Loadside", role: .destructive) { Task { await store.remove(app, uninstall: true); dismiss() } }
            Button("Only Stop Refreshing") { Task { await store.remove(app, uninstall: false); dismiss() } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Deleting it from the device frees one of the 3 app slots free accounts get.")
        }
    }

    // MARK: - GitHub releases

    private func updateAvailableBanner(app: AppRecord, release: ReleaseInfo, connected: Bool) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: "arrow.down.circle.fill").foregroundStyle(.tint)
                Text("Version \(release.tag) is available").font(.callout.weight(.semibold))
                if release.prerelease { Tag(text: "prerelease", color: .orange) }
                Spacer()
                if let pub = release.publishedAt {
                    Text(Fmt.rel(pub)).font(.caption).foregroundStyle(.secondary)
                }
            }
            HStack(spacing: 8) {
                Text("\(release.asset.name) · \(Fmt.bytes(Int(clamping: release.asset.size)))")
                    .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                Spacer()
                if let url = GitHubURL.safeWebURL(release.htmlUrl) {
                    Button("View on GitHub") { NSWorkspace.shared.open(url) }
                        .buttonStyle(.link).font(.caption)
                }
            }
            if let body = release.body, !body.isEmpty {
                DisclosureGroup("Release notes", isExpanded: $showNotes) {
                    ScrollView {
                        Text(body)
                            .font(.caption).foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .textSelection(.enabled)
                    }
                    .frame(maxHeight: 110)
                    .padding(.top, 4)
                }
                .font(.caption)
            }
            if !connected {
                Text("Connect \(app.deviceName.isEmpty ? "your device" : app.deviceName) to install this update.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.accentColor.opacity(0.09), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(Color.accentColor.opacity(0.22)))
    }

    @ViewBuilder
    private func githubSection(app: AppRecord, job: JobInfo?) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("GitHub Releases").font(.caption.weight(.semibold)).foregroundStyle(.secondary).padding(.horizontal, 4)
            if editingGitHub {
                githubEditor(app: app)
            } else if let gh = app.github {
                VStack(spacing: 0) {
                    row("Repository") {
                        HStack(spacing: 8) {
                            if let url = URL(string: "https://github.com/\(gh.repo)/releases") {
                                Button(gh.repo) { NSWorkspace.shared.open(url) }
                                    .buttonStyle(.link)
                            } else {
                                Text(gh.repo)
                            }
                            Menu {
                                Button("Edit Repository & Filter…") { beginEditing(gh) }
                                Divider()
                                Button("Disconnect GitHub", role: .destructive) {
                                    Task { await store.setAppGitHub(app, disconnect: true) }
                                }
                            } label: {
                                Image(systemName: "ellipsis.circle").foregroundStyle(.secondary)
                            }
                            .menuStyle(.borderlessButton)
                            .menuIndicator(.hidden)
                            .fixedSize()
                        }
                    }
                    Divider()
                    VStack(alignment: .leading, spacing: 4) {
                        HStack {
                            Text("Updates").foregroundStyle(.secondary)
                            Spacer()
                            Picker("", selection: Binding(
                                get: { gh.policy },
                                set: { pol in Task { await store.setAppGitHub(app, policy: pol) } }
                            )) {
                                ForEach(UpdatePolicy.allCases) { p in Text(p.label).tag(p) }
                            }
                            .labelsHidden()
                            .fixedSize()
                        }
                        Text(gh.policy.explanation)
                            .font(.caption).foregroundStyle(.tertiary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .font(.callout)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    Divider()
                    row("Include prereleases") {
                        Toggle("", isOn: Binding(
                            get: { gh.includePrereleases },
                            set: { on in Task { await store.setAppGitHub(app, includePrereleases: on) } }
                        ))
                        .labelsHidden().toggleStyle(.switch).controlSize(.small)
                    }
                    if let pat = gh.assetPattern, !pat.isEmpty {
                        Divider()
                        row("IPA filter") {
                            Text(pat).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1)
                        }
                    }
                    Divider()
                    row("Last checked") {
                        HStack(spacing: 8) {
                            Text(gh.lastCheckedAt.map { Fmt.rel($0) } ?? "Never").foregroundStyle(.secondary)
                            Button {
                                Task {
                                    checkingUpdates = true
                                    await store.checkUpdates(app: app, force: true)
                                    checkingUpdates = false
                                }
                            } label: {
                                if checkingUpdates {
                                    ProgressView().controlSize(.mini)
                                } else {
                                    Text("Check Now")
                                }
                            }
                            .controlSize(.small)
                            .disabled(checkingUpdates || job != nil)
                        }
                    }
                }
                .surface(cornerRadius: 14, padding: nil)

                if let err = gh.lastCheckError {
                    FailureView(failure: err)
                }
            } else {
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Track a GitHub repository").font(.callout.weight(.medium))
                        Text("Automatically fetch new .ipa releases when updates come out.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Connect…") { beginEditing(nil) }
                        .controlSize(.small)
                }
                .surface(cornerRadius: 14, padding: 14)
            }
        }
    }

    private func githubEditor(app: AppRecord) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                TextField("https://github.com/owner/repo or owner/repo", text: $repoInput)
                    .textFieldStyle(.roundedBorder)
                    .disabled(lookingUpRepo || savingGitHub)
                    .onSubmit { Task { await verifyRepo() } }
                Button {
                    Task { await verifyRepo() }
                } label: {
                    if lookingUpRepo {
                        ProgressView().controlSize(.small).frame(width: 48)
                    } else {
                        Text("Verify")
                    }
                }
                .controlSize(.small)
                .disabled(repoInput.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || lookingUpRepo || savingGitHub)
            }

            if inspectedAssets.count > 1 {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Multiple .ipa assets found: pick one to track").font(.caption).foregroundStyle(.secondary)
                    Picker("", selection: Binding(
                        get: {
                            let pats = inspectedAssets.map { suggestedPatterns[$0.name] ?? $0.name }
                            return pats.contains(assetPatternInput) ? assetPatternInput : "__custom__"
                        },
                        set: { newVal in
                            if newVal != "__custom__" { assetPatternInput = newVal }
                        }
                    )) {
                        ForEach(inspectedAssets) { a in
                            let pat = suggestedPatterns[a.name] ?? a.name
                            Text("\(a.name) (\(pat))").tag(pat)
                        }
                        if !inspectedAssets.map({ suggestedPatterns[$0.name] ?? $0.name }).contains(assetPatternInput) {
                            Text("Custom filter").tag("__custom__")
                        }
                    }
                    .labelsHidden()
                }
                TextField("Optional .ipa name filter (e.g. *arm64.ipa)", text: $assetPatternInput)
                    .textFieldStyle(.roundedBorder)
                    .font(.callout)
            } else if showAssetFilter || !assetPatternInput.isEmpty {
                TextField("Optional .ipa name filter (e.g. *arm64.ipa)", text: $assetPatternInput)
                    .textFieldStyle(.roundedBorder)
                    .font(.callout)
            }

            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Text("When a new version comes out").font(.callout).foregroundStyle(.secondary)
                    Spacer()
                    Picker("", selection: $policyInput) {
                        ForEach(UpdatePolicy.allCases) { p in Text(p.label).tag(p) }
                    }
                    .labelsHidden()
                    .fixedSize()
                }
                Text(policyInput.explanation)
                    .font(.caption).foregroundStyle(.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Toggle("Include prereleases", isOn: $prereleasesInput)
                .font(.callout)
                .onChange(of: prereleasesInput) { _, _ in
                    inspectedAssets = []
                    suggestedPatterns = [:]
                }

            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "exclamationmark.shield.fill")
                    .foregroundStyle(.orange)
                    .padding(.top, 1)
                Text("Only connect repositories from developers you trust. Future releases will be downloaded and signed with your Apple ID, and an untrusted release could access this app's data or run untrusted code on your device.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.orange.opacity(0.09), in: RoundedRectangle(cornerRadius: 10, style: .continuous))

            if let repoError {
                FailureView(failure: repoError)
            }

            HStack {
                if inspectedAssets.count <= 1 && !showAssetFilter && assetPatternInput.isEmpty {
                    Button("Filter .ipa name…") { showAssetFilter = true }
                        .buttonStyle(.link)
                        .font(.caption)
                }
                Spacer()
                Button("Cancel") {
                    editingGitHub = false
                    showAssetFilter = false
                    repoError = nil
                    inspectedAssets = []
                }
                .disabled(savingGitHub)
                Button(savingGitHub ? "Saving…" : "Save") {
                    let normalized = GitHubURL.displayRepo(repoInput)
                    let repoChanged = app.github == nil || app.github?.repo.caseInsensitiveCompare(normalized) != .orderedSame
                    if repoChanged {
                        confirmTrustRepo = true
                    } else {
                        Task { await saveGitHub(app) }
                    }
                }
                .primaryButton()
                .disabled(repoInput.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || savingGitHub)
            }
            .controlSize(.small)
        }
        .surface(cornerRadius: 14, padding: 14)
        .alert("Only connect trusted repositories", isPresented: $confirmTrustRepo) {
            Button("Cancel", role: .cancel) {}
            Button("Trust & Connect") { Task { await saveGitHub(app) } }
        } message: {
            Text("Loadside will download future .ipa releases from \(GitHubURL.displayRepo(repoInput)) and sign them with your Apple ID. Only add repositories from developers you trust, as a malicious or compromised release could access your app data or run untrusted code on your device.")
        }
    }

    private func beginEditing(_ existing: GitHubSource?) {
        repoInput = existing?.repo ?? ""
        assetPatternInput = existing?.assetPattern ?? ""
        policyInput = existing?.policy ?? .onRefresh
        prereleasesInput = existing?.includePrereleases ?? false
        inspectedAssets = []
        showAssetFilter = !(existing?.assetPattern?.isEmpty ?? true)
        repoError = nil
        editingGitHub = true
    }

    private func verifyRepo() async {
        let trimmed = repoInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        lookingUpRepo = true
        repoError = nil
        defer { lookingUpRepo = false }
        do {
            let res = try await store.githubInspect(repo: trimmed, includePrereleases: prereleasesInput)
            repoInput = res.repo
            inspectedAssets = res.assets
            suggestedPatterns = res.suggestedPatterns ?? [:]
            if res.assets.count > 1 && assetPatternInput.isEmpty {
                assetPatternInput = suggestedPatterns[res.release.asset.name] ?? res.release.asset.name
            }
        } catch {
            inspectedAssets = []
            suggestedPatterns = [:]
            repoError = (error as? Failure) ?? Failure.local(error.localizedDescription)
        }
    }

    private func saveGitHub(_ app: AppRecord) async {
        let trimmed = repoInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        savingGitHub = true
        repoError = nil
        defer { savingGitHub = false }
        let pat = assetPatternInput.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            try await store.setAppGitHubThrowing(
                app,
                repo: trimmed,
                policy: policyInput,
                includePrereleases: prereleasesInput,
                assetPattern: .some(pat.isEmpty ? nil : pat)
            )
            editingGitHub = false
            inspectedAssets = []
            suggestedPatterns = [:]
        } catch {
            repoError = (error as? Failure) ?? Failure.local(error.localizedDescription)
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
