import SwiftUI
import UniformTypeIdentifiers

struct InstallView: View {
    @Environment(Store.self) private var store
    var goToAccount: () -> Void

    @State private var path: String?
    @State private var inspect: InspectResult?
    @State private var inspectError: Failure?
    @State private var options = InstallOptions()
    @State private var deviceUdid: String = ""
    @State private var jobId: String?
    @State private var dropTargeted = false
    @State private var busy = false
    /// Identifies the current selection, so a slow inspect of an earlier
    /// file can't overwrite the details of the one picked after it.
    @State private var loadToken = UUID()

    /// Apps that refresh themselves with your signing key (see daemon).
    private static let keyReceivers: Set<String> = ["com.rileytestut.AltStore", "com.SideStore.SideStore", "app.stik.store", "com.kdt.livecontainer"]

    private static let ipaType = UTType(filenameExtension: "ipa") ?? .data

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                header
                if store.account == nil { signInCallout }
                if path == nil {
                    dropZone
                } else {
                    appCard
                    if inspect != nil {
                        optionsCard
                        injectCard
                        actionBar
                    }
                }
                if let job = store.job(jobId) { jobCard(job) }
            }
            .padding(20)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity)
        }
        .onDrop(of: [.fileURL], isTargeted: $dropTargeted) { providers in handleDrop(providers) }
        .onChange(of: store.devices) { _, devs in
            if !devs.contains(where: { $0.udid == deviceUdid }) { deviceUdid = devs.first?.udid ?? "" }
        }
        .onAppear { if deviceUdid.isEmpty { deviceUdid = store.devices.first?.udid ?? "" } }
        .task(id: store.account?.email) { if store.account != nil && store.appIds == nil { await store.loadAppIds() } }
    }

    private var header: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text("Install an app").font(.title2.weight(.semibold))
                Text("Sign an IPA with your Apple ID and install it. Free accounts' apps last 7 days; Loadside refreshes them automatically.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            Spacer()
            if path != nil {
                Button("Choose Another…") { choose() }
            }
        }
    }

    private var signInCallout: some View {
        Card {
            HStack {
                Image(systemName: "person.crop.circle.badge.exclamationmark").font(.title2).foregroundStyle(.orange)
                VStack(alignment: .leading) {
                    Text("Sign in first").font(.headline)
                    Text("An Apple ID (free is fine) is needed to sign apps.").font(.callout).foregroundStyle(.secondary)
                }
                Spacer()
                Button("Sign In…", action: goToAccount).buttonStyle(.borderedProminent)
            }
        }
    }

    private var dropZone: some View {
        Button(action: choose) {
            VStack(spacing: 12) {
                Image(systemName: "arrow.down.app").font(.system(size: 44, weight: .light))
                Text("Drop an .ipa here").font(.title3.weight(.medium))
                Text("or click to choose a file").font(.callout).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, minHeight: 240)
            .background(
                RoundedRectangle(cornerRadius: 16)
                    .strokeBorder(style: StrokeStyle(lineWidth: 2, dash: [8, 6]))
                    .foregroundStyle(dropTargeted ? Color.accentColor : Color.secondary.opacity(0.4))
            )
            .background(dropTargeted ? Color.accentColor.opacity(0.06) : .clear, in: RoundedRectangle(cornerRadius: 16))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    @ViewBuilder private var appCard: some View {
        Card {
            if let i = inspect?.info {
                HStack(alignment: .top, spacing: 14) {
                    RoundedRectangle(cornerRadius: 12).fill(.tint.opacity(0.15)).frame(width: 52, height: 52)
                        .overlay(Text(String(i.name.prefix(1))).font(.title.weight(.semibold)).foregroundStyle(.tint))
                    VStack(alignment: .leading, spacing: 3) {
                        Text(i.name).font(.title3.weight(.semibold))
                        Text("\(i.bundleId) · \(i.version ?? "?")\(i.build.map { " (\($0))" } ?? "")")
                            .font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                        HStack(spacing: 6) {
                            Tag(text: Fmt.bytes(i.sizeBytes))
                            if let m = i.minimumOs { Tag(text: "iOS \(m)+") }
                            if i.extensionCount > 0 { Tag(text: "\(i.extensionCount) extension\(i.extensionCount == 1 ? "" : "s")") }
                            if i.hasWatchApp { Tag(text: "Watch app") }
                            if i.encrypted == true { Tag(text: "Encrypted", color: .red) }
                        }
                    }
                    Spacer()
                    Button { reset() } label: { Image(systemName: "xmark.circle.fill") }
                        .buttonStyle(.plain).foregroundStyle(.secondary)
                }
                if i.encrypted == true {
                    FailureView(failure: Failure(code: "encrypted_ipa", message: "This IPA is still FairPlay-encrypted.",
                        hint: "It was copied from the App Store without decrypting and will crash on launch. Use a decrypted IPA.", detail: nil))
                }
                appIdBudget(i)
            } else if let e = inspectError {
                FailureView(failure: e)
            } else {
                HStack { ProgressView().controlSize(.small); Text("Reading \(URL(fileURLWithPath: path ?? "").lastPathComponent)…") }
            }
        }
    }

    /// Exact App IDs this install needs, as "<id>.<TEAMID>" (how the signer registers them).
    private func neededIdentifiers(_ i: IpaInfo) -> [String] {
        let base = options.bundleId.flatMap { $0.isEmpty ? nil : $0 } ?? i.bundleId
        var ids = [base]
        if options.extensions == .registerAll {
            for ext in i.subBundles where ext.kind == "extension" {
                if ext.bundleId.hasPrefix(i.bundleId + ".") {
                    ids.append(base + String(ext.bundleId.dropFirst(i.bundleId.count)))
                } else {
                    ids.append(base + "." + (ext.bundleId.split(separator: ".").last.map(String.init) ?? "ext"))
                }
            }
        }
        return ids
    }

    @ViewBuilder private func appIdBudget(_ i: IpaInfo) -> some View {
        if let avail = store.appIds?.availableQuantity, store.account?.isFree != false {
            let team = store.account?.teamId ?? ""
            let registered = Set(store.appIds?.appIds.map(\.identifier) ?? [])
            // An ID counts as existing only on an exact match (with or without the team suffix).
            let missing = neededIdentifiers(i).filter { id in !registered.contains(id) && !registered.contains("\(id).\(team)") }
            let short = missing.count > avail
            HStack(spacing: 6) {
                Image(systemName: short ? "exclamationmark.circle.fill" : "checkmark.circle")
                    .foregroundStyle(short ? .orange : .green)
                Text(missing.isEmpty ? "Reuses App IDs already registered for this app." : "Needs \(missing.count) new App ID\(missing.count == 1 ? "" : "s") · \(avail) available this week")
                    .font(.callout)
                if short && i.extensionCount > 0 && options.extensions == .registerAll {
                    Button("Strip extensions") { options.extensions = .strip }.controlSize(.small)
                }
            }
        }
    }

    private var optionsCard: some View {
        Card {
            Text("Options").font(.headline)
            Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 14, verticalSpacing: 10) {
                GridRow {
                    Text("Device").foregroundStyle(.secondary)
                    if store.devices.isEmpty {
                        Label("Connect your iPhone or iPad with a cable and unlock it", systemImage: "cable.connector")
                            .foregroundStyle(.orange)
                    } else {
                        Picker("", selection: $deviceUdid) {
                            ForEach(store.devices) { d in
                                Text("\(d.name)  ·  iOS \(d.iosVersion ?? "?")  ·  \(d.connection == "wifi" ? "Wi-Fi" : "USB")").tag(d.udid)
                            }
                        }
                        .labelsHidden().frame(maxWidth: 360)
                    }
                }
                GridRow {
                    Text("Bundle ID").foregroundStyle(.secondary)
                    TextField(inspect?.info.bundleId ?? "", text: Binding(get: { options.bundleId ?? "" }, set: { options.bundleId = $0.isEmpty ? nil : $0 }))
                        .textFieldStyle(.roundedBorder).frame(maxWidth: 360)
                }
                GridRow {
                    Text("Name").foregroundStyle(.secondary)
                    TextField(inspect?.info.name ?? "", text: Binding(get: { options.displayName ?? "" }, set: { options.displayName = $0.isEmpty ? nil : $0 }))
                        .textFieldStyle(.roundedBorder).frame(maxWidth: 360)
                }
                if (inspect?.info.extensionCount ?? 0) > 0 {
                    GridRow(alignment: .top) {
                        Text("Extensions").foregroundStyle(.secondary)
                        VStack(alignment: .leading, spacing: 4) {
                            Picker("", selection: $options.extensions) {
                                ForEach(ExtensionsMode.allCases) { Text($0.label).tag($0) }
                            }
                            .pickerStyle(.segmented).labelsHidden().frame(maxWidth: 360)
                            Text(options.extensions.explanation).font(.caption).foregroundStyle(.secondary).frame(maxWidth: 420, alignment: .leading)
                        }
                    }
                }
            }
            Divider()
            VStack(alignment: .leading, spacing: 6) {
                Toggle("Refresh automatically before it expires", isOn: $options.autoRefresh)
                Toggle("Allow on any device model (remove UISupportedDevices)", isOn: $options.removeSupportedDevices)
                if inspect?.info.hasWatchApp == true {
                    Toggle("Remove Apple Watch app (saves App IDs)", isOn: $options.stripWatchApp)
                }
                Toggle("Show app's Documents in Files and Finder", isOn: $options.fileSharing)
                Toggle("Request increased memory limit", isOn: $options.increasedMemoryLimit)
                if inspect?.info.encrypted == true {
                    Toggle("Install anyway even though it's encrypted", isOn: $options.allowEncrypted)
                }
                if needsKeyConsent {
                    VStack(alignment: .leading, spacing: 3) {
                        Toggle("Allow this app to receive my signing key", isOn: $options.allowSigningKey)
                        Text("AltStore, SideStore and LiveContainer refresh themselves with your certificate and its private key, so it gets copied into the app. Only allow it for a copy from the official source, and never share the signed IPA.")
                            .font(.caption).foregroundStyle(.secondary).frame(maxWidth: 460, alignment: .leading)
                    }
                }
            }
        }
    }

    private var injectCard: some View {
        Card {
            HStack {
                Text("Tweaks").font(.headline)
                Text("optional").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Add…") { addInjection() }
            }
            if options.inject.isEmpty {
                Text("Inject .dylib, .framework, .deb or .bundle files. Tweaks that need CydiaSubstrate also need ElleKit (libellekit.dylib) or CydiaSubstrate.framework added here.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            ForEach(options.inject, id: \.self) { p in
                HStack {
                    Image(systemName: p.hasSuffix(".deb") ? "shippingbox" : "puzzlepiece.extension")
                    Text(URL(fileURLWithPath: p).lastPathComponent)
                    Spacer()
                    Button { options.inject.removeAll { $0 == p } } label: { Image(systemName: "minus.circle") }.buttonStyle(.plain)
                }
                .font(.callout)
            }
        }
    }

    private var actionBar: some View {
        HStack {
            Button("Export Signed IPA…") { Task { await export() } }
                .disabled(!canSign || busy)
            Spacer()
            Button {
                Task { await install() }
            } label: {
                Label("Install", systemImage: "arrow.down.circle.fill").frame(minWidth: 110)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .keyboardShortcut(.defaultAction)
            .disabled(!canSign || deviceUdid.isEmpty || busy)
        }
    }

    private var needsKeyConsent: Bool {
        let id = options.bundleId.flatMap { $0.isEmpty ? nil : $0 } ?? inspect?.info.bundleId ?? ""
        return Self.keyReceivers.contains(id) || store.job(jobId)?.error?.code == "signing_key_requested"
    }

    private var canSign: Bool {
        guard let i = inspect?.info else { return false }
        return store.account != nil && (i.encrypted != true || options.allowEncrypted) && !(store.job(jobId)?.isActive ?? false)
    }

    @ViewBuilder private func jobCard(_ job: JobInfo) -> some View {
        Card {
            switch job.status {
            case .queued, .running:
                JobProgressView(job: job)
            case .succeeded:
                Label(job.kind == .export ? "Signed IPA saved" : "Installed", systemImage: "checkmark.circle.fill")
                    .foregroundStyle(.green).font(.headline)
                if let out = store.exportDestinations[job.id] ?? job.outputPath.map({ URL(fileURLWithPath: $0) }) {
                    Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([out]) }
                }
                if job.kind == .install, let dev = store.devices.first(where: { $0.udid == job.deviceUdid }), dev.developerMode == false {
                    Text("Before opening the app: on \(dev.name) go to Settings → Privacy & Security → Developer Mode, turn it on and restart.")
                        .font(.callout).foregroundStyle(.secondary)
                } else if job.kind == .install {
                    Text("If iOS says \"Untrusted Developer\": Settings → General → VPN & Device Management → trust your Apple ID.")
                        .font(.callout).foregroundStyle(.secondary)
                }
            case .failed:
                if let e = job.error { FailureView(failure: e) }
            case .cancelled:
                Text("Cancelled").foregroundStyle(.secondary)
            }
            ForEach(job.warnings, id: \.self) { w in
                Label(w, systemImage: "exclamationmark.circle").font(.caption).foregroundStyle(.orange)
            }
        }
    }

    // MARK: Actions

    private func reset() {
        // A running job may still be reading these; the daemon cleans up
        // leftovers after a few days.
        if !(store.job(jobId)?.isActive ?? false) {
            if let path { Store.unstage(path) }
            for p in options.inject { Store.unstage(p) }
        }
        path = nil; inspect = nil; inspectError = nil; jobId = nil
        options = InstallOptions()
        loadToken = UUID()
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [Self.ipaType, .applicationBundle]
        panel.allowsMultipleSelection = false
        panel.message = "Choose an IPA to install"
        if panel.runModal() == .OK, let url = panel.url { load(url) }
    }

    private func addInjection() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [UTType(filenameExtension: "dylib"), UTType(filenameExtension: "deb"), .bundle, .framework]
            .compactMap { $0 }
        panel.canChooseDirectories = true
        panel.treatsFilePackagesAsDirectories = false
        panel.allowsMultipleSelection = true
        if panel.runModal() == .OK {
            for u in panel.urls { addTweak(u) }
        }
    }

    private func addTweak(_ url: URL) {
        let token = loadToken
        Task {
            do {
                let staged = try await Store.stage(url)
                guard token == loadToken else { Store.unstage(staged.path); return }
                if !options.inject.contains(where: { URL(fileURLWithPath: $0).lastPathComponent == url.lastPathComponent }) {
                    options.inject.append(staged.path)
                }
            } catch {
                store.show(Failure.local("Couldn't read \(url.lastPathComponent).", hint: error.localizedDescription))
            }
        }
    }

    private func handleDrop(_ providers: [NSItemProvider]) -> Bool {
        for p in providers {
            _ = p.loadObject(ofClass: URL.self) { url, _ in
                guard let url else { return }
                Task { @MainActor in
                    let ext = url.pathExtension.lowercased()
                    if ["dylib", "deb", "framework", "bundle"].contains(ext), path != nil {
                        addTweak(url)
                    } else if ext == "ipa" || ext == "app" {
                        load(url)
                    } else {
                        store.show(Failure.local("That's not an .ipa file.", hint: "Drop an .ipa (or tweaks once an app is chosen)."))
                    }
                }
            }
        }
        return true
    }

    private func load(_ url: URL) {
        reset()
        let token = loadToken
        path = url.path
        Task {
            do {
                // Hand the daemon a copy inside Loadside's own folder.
                let staged = try await Store.stage(url)
                guard token == loadToken else { Store.unstage(staged.path); return }
                path = staged.path
                let r = try await store.inspect(path: staged.path, options: options)
                guard token == loadToken else { return }
                inspect = r
            } catch {
                guard token == loadToken else { return }
                inspectError = (error as? Failure) ?? Failure.local(error.localizedDescription)
            }
        }
    }

    private func install() async {
        guard let path else { return }
        busy = true
        defer { busy = false }
        jobId = await store.run {
            try await store.install(InstallParams(path: path, deviceUdid: deviceUdid, options: options, exportPath: nil, install: true))
        }
    }

    private func export() async {
        guard let path, let info = inspect?.info, !busy else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [Self.ipaType]
        panel.nameFieldStringValue = "\(info.name)-signed.ipa"
        guard panel.runModal() == .OK, let out = panel.url else { return }
        busy = true
        defer { busy = false }
        // The daemon writes into Loadside's folder; the app moves it to `out`.
        guard let staging = try? Store.exportStaging(name: out.lastPathComponent) else { return }
        jobId = await store.run {
            try await store.install(InstallParams(path: path, deviceUdid: deviceUdid.isEmpty ? nil : deviceUdid, options: options, exportPath: staging.path, install: false))
        }
        if let jobId { store.exportDestinations[jobId] = out }
    }
}
