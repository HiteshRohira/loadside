import SwiftUI

/// Enter a GitHub repository -> preview its latest .ipa release -> download and hand off to InstallSheet.
struct GitHubInstallSheet: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    let onReady: (URL, GitHubSource) -> Void

    @State private var repoInput = ""
    @State private var includePrereleases = false
    @State private var policy: UpdatePolicy = .onRefresh
    @State private var inspected: GitHubInspectResult?
    @State private var selectedAssetName = ""
    @State private var showNotes = false
    @State private var loading = false
    @State private var downloading = false
    @State private var confirmTrustRepo = false
    @State private var lookupTask: Task<Void, Never>?
    @State private var downloadTask: Task<Void, Never>?
    @State private var error: Failure?

    var body: some View {
        VStack(spacing: 0) {
            FittingScrollView(maxHeight: 500) {
                VStack(alignment: .leading, spacing: 18) {
                    header
                    repoField
                    if let error {
                        FailureView(failure: error)
                    }
                    if let inspected {
                        releaseCard(inspected)
                        policySection
                        trustWarning
                    }
                }
                .padding(24)
            }
            Divider()
            footer.padding(16)
        }
        .frame(width: 500)
        .alert("Only install from trusted repositories", isPresented: $confirmTrustRepo) {
            Button("Cancel", role: .cancel) {}
            Button("Trust & Continue") {
                if let inspected {
                    downloadTask?.cancel()
                    downloadTask = Task { await downloadAndContinue(inspected) }
                }
            }
        } message: {
            let repoName = inspected?.repo ?? GitHubURL.displayRepo(repoInput)
            Text("Loadside will download .ipa releases from \(repoName) and sign them with your Apple ID. Only add repositories from developers you trust, as a malicious or compromised release could access your app data or run untrusted code on your device.")
        }
        .onDisappear {
            lookupTask?.cancel()
            downloadTask?.cancel()
        }
    }

    private var header: some View {
        HStack(spacing: 14) {
            Image(systemName: "arrow.down.circle.fill")
                .font(.system(size: 38))
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 3) {
                Text("Install from GitHub").font(.title2.weight(.semibold))
                Text("Fetch the latest .ipa release and track future updates automatically.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
    }

    private var repoField: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                TextField("https://github.com/owner/repo or owner/repo", text: $repoInput)
                    .textFieldStyle(.roundedBorder)
                    .disabled(loading || downloading)
                    .onSubmit { startLookup() }
                    .onChange(of: repoInput) { _, newValue in
                        if let cur = inspected,
                           GitHubURL.displayRepo(newValue).caseInsensitiveCompare(cur.repo) != .orderedSame {
                            inspected = nil
                        }
                    }
                Button {
                    startLookup()
                } label: {
                    if loading {
                        ProgressView().controlSize(.small).frame(width: 56)
                    } else {
                        Text("Look Up").frame(minWidth: 56)
                    }
                }
                .disabled(repoInput.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || loading || downloading)
            }
            Toggle("Include prereleases", isOn: $includePrereleases)
                .font(.callout)
                .disabled(loading || downloading)
                .onChange(of: includePrereleases) { _, _ in
                    if inspected != nil { startLookup() }
                }
        }
    }

    private func releaseCard(_ res: GitHubInspectResult) -> some View {
        let rel = res.release
        let chosen = res.assets.first { $0.name == selectedAssetName } ?? rel.asset
        return VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(res.repo).font(.headline)
                Tag(text: rel.tag, color: .accentColor)
                if rel.prerelease { Tag(text: "prerelease", color: .orange) }
                Spacer()
                if let pub = rel.publishedAt {
                    Text(Fmt.rel(pub)).font(.caption).foregroundStyle(.secondary)
                }
            }
            if let title = rel.name, !title.isEmpty, title != rel.tag {
                Text(title).font(.callout).foregroundStyle(.secondary)
            }

            if res.assets.count > 1 {
                VStack(alignment: .leading, spacing: 4) {
                    Text("IPA file").font(.caption).foregroundStyle(.secondary)
                    Picker("", selection: $selectedAssetName) {
                        ForEach(res.assets) { a in
                            Text("\(a.name) (\(Fmt.bytes(Int(clamping: a.size))))").tag(a.name)
                        }
                    }
                    .labelsHidden()
                    .disabled(downloading)
                }
            } else {
                HStack(spacing: 6) {
                    Image(systemName: "doc.zipper").foregroundStyle(.secondary)
                    Text(chosen.name).font(.callout).lineLimit(1)
                    Spacer()
                    Text(Fmt.bytes(Int(clamping: chosen.size))).font(.caption).foregroundStyle(.secondary)
                }
            }

            if let body = rel.body, !body.isEmpty {
                DisclosureGroup("Release notes", isExpanded: $showNotes) {
                    ScrollView {
                        Text(body)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .textSelection(.enabled)
                    }
                    .frame(maxHeight: 120)
                    .padding(.top, 4)
                }
                .font(.callout)
            }

            if let url = GitHubURL.safeWebURL(rel.htmlUrl) {
                Button {
                    NSWorkspace.shared.open(url)
                } label: {
                    Label("View release on GitHub", systemImage: "arrow.up.right.square")
                        .font(.caption)
                }
                .buttonStyle(.link)
            }
        }
        .surface(cornerRadius: 14, padding: 14)
    }

    private var policySection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("When a new version is released").font(.callout.weight(.medium))
            Picker("", selection: $policy) {
                ForEach(UpdatePolicy.allCases) { p in
                    Text(p.label).tag(p)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .disabled(downloading)
            Text(policy.explanation)
                .font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var trustWarning: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.shield.fill")
                .foregroundStyle(.orange)
                .padding(.top, 1)
            Text("Only add repositories from developers you trust. Future releases will be downloaded and signed with your Apple ID, and an untrusted release could access your app data or run untrusted code on your device.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.orange.opacity(0.09), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    private var footer: some View {
        HStack {
            if downloading {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Downloading .ipa from GitHub…").font(.callout).foregroundStyle(.secondary)
                }
            }
            Spacer()
            Button("Cancel") {
                lookupTask?.cancel()
                downloadTask?.cancel()
                dismiss()
            }
            .keyboardShortcut(.cancelAction)
            if inspected != nil {
                Button {
                    confirmTrustRepo = true
                } label: {
                    Text(downloading ? "Downloading…" : "Continue").frame(minWidth: 90)
                }
                .keyboardShortcut(.defaultAction)
                .primaryButton()
                .disabled(loading || downloading)
            } else {
                Button("Look Up") { startLookup() }
                    .keyboardShortcut(.defaultAction)
                    .primaryButton()
                    .disabled(repoInput.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || loading)
            }
        }
        .controlSize(.large)
    }

    private func startLookup() {
        lookupTask?.cancel()
        lookupTask = Task { await lookup() }
    }

    private func lookup() async {
        let trimmed = repoInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        loading = true
        error = nil
        defer { loading = false }
        do {
            let res = try await store.githubInspect(repo: trimmed, includePrereleases: includePrereleases)
            guard !Task.isCancelled else { return }
            inspected = res
            selectedAssetName = res.release.asset.name
        } catch {
            guard !Task.isCancelled else { return }
            inspected = nil
            self.error = (error as? Failure) ?? Failure.local(error.localizedDescription)
        }
    }

    private func downloadAndContinue(_ inspected: GitHubInspectResult) async {
        downloading = true
        error = nil
        defer { downloading = false }
        do {
            let chosenAsset = inspected.assets.first { $0.name == selectedAssetName } ?? inspected.release.asset
            let dl = try await store.githubDownload(
                repo: inspected.repo,
                includePrereleases: includePrereleases,
                assetName: chosenAsset.name,
                releaseId: inspected.release.releaseId,
                assetId: chosenAsset.id
            )
            if Task.isCancelled {
                Store.unstage(dl.path)
                return
            }
            let gh = GitHubSource(
                repo: dl.repo,
                repoId: dl.repoId ?? inspected.repoId,
                assetPattern: dl.assetPattern,
                includePrereleases: includePrereleases,
                policy: policy,
                installedReleaseId: dl.release.releaseId,
                installedTag: dl.release.tag,
                installedAssetName: dl.release.asset.name,
                availableRelease: nil,
                lastCheckedAt: Int(Date().timeIntervalSince1970),
                etag: nil,
                lastCheckError: nil,
                lastUpdateAttemptAt: nil,
                consecutiveUpdateFailures: 0
            )
            onReady(URL(fileURLWithPath: dl.path), gh)
        } catch {
            guard !Task.isCancelled else { return }
            self.error = (error as? Failure) ?? Failure.local(error.localizedDescription)
        }
    }
}
