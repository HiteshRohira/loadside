import SwiftUI
import UniformTypeIdentifiers

/// Pick a file → see what it is → Install. Everything optional is folded away.
struct InstallSheet: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    let source: URL

    @State private var staged: String?
    @State private var inspect: InspectResult?
    @State private var inspectError: Failure?
    @State private var options = InstallOptions()
    @State private var deviceUdid = ""
    @State private var jobId: String?
    @State private var busy = false
    @State private var showOptions = false
    @State private var dropTargeted = false

    /// Apps that refresh themselves with your signing key (see daemon).
    private static let keyReceivers: Set<String> = ["com.rileytestut.AltStore", "com.SideStore.SideStore", "app.stik.store", "com.kdt.livecontainer"]

    private var job: JobInfo? { store.job(jobId) }

    var body: some View {
        VStack(spacing: 0) {
            FittingScrollView(maxHeight: 460) {
                Group {
                    if let job, job.kind == .install || job.kind == .export, job.status != .failed {
                        progress(job)
                    } else {
                        form
                    }
                }
                .padding(24)
            }
            Divider()
            footer.padding(16)
        }
        .frame(width: 520)
        .presentsPrompts()
        .onDrop(of: [.fileURL], isTargeted: $dropTargeted, perform: handleDrop)
        .task { await load() }
        .onAppear { if deviceUdid.isEmpty { deviceUdid = store.devices.first?.udid ?? "" } }
        .onChange(of: store.devices) { _, devs in
            if !devs.contains(where: { $0.udid == deviceUdid }) { deviceUdid = devs.first?.udid ?? "" }
        }
        .onDisappear(perform: cleanUp)
    }

    // MARK: Form

    @ViewBuilder private var form: some View {
        VStack(alignment: .leading, spacing: 18) {
            header
            if inspect != nil {
                if inspect?.info.encrypted == true {
                    FailureView(failure: Failure(code: "encrypted_ipa", message: "This IPA is still FairPlay-encrypted.",
                        hint: "It was copied from the App Store without decrypting and will crash on launch. Use a decrypted IPA.", detail: nil))
                }
                if let e = job?.error { FailureView(failure: e) }
                deviceRow
                budgetRow
                if needsKeyConsent { keyConsent }
                optionsDisclosure
            } else if let e = inspectError {
                FailureView(failure: e)
            }
        }
    }

    private var header: some View {
        HStack(spacing: 16) {
            if let i = inspect?.info {
                AppIconView(path: staged, name: i.name, size: 64)
                VStack(alignment: .leading, spacing: 4) {
                    Text(i.name).font(.title2.weight(.semibold)).lineLimit(1)
                    Text([i.version.map { "Version \($0)" }, Fmt.bytes(i.sizeBytes), i.minimumOs.map { "iOS \($0)+" }]
                        .compactMap { $0 }.joined(separator: " · "))
                        .font(.callout).foregroundStyle(.secondary)
                    Text(i.bundleId).font(.caption).foregroundStyle(.tertiary).textSelection(.enabled)
                }
            } else if inspectError == nil {
                RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Color.primary.opacity(0.06))
                    .frame(width: 64, height: 64)
                    .overlay(ProgressView().controlSize(.small))
                VStack(alignment: .leading, spacing: 4) {
                    Text(source.deletingPathExtension().lastPathComponent).font(.title2.weight(.semibold)).lineLimit(1)
                    Text("Reading the app…").font(.callout).foregroundStyle(.secondary)
                }
            } else {
                Image(systemName: "questionmark.app.dashed").font(.system(size: 48, weight: .light)).foregroundStyle(.secondary)
                Text(source.lastPathComponent).font(.title3.weight(.semibold)).lineLimit(1)
            }
            Spacer(minLength: 0)
        }
    }

    @ViewBuilder private var deviceRow: some View {
        HStack(spacing: 10) {
            Image(systemName: store.devices.isEmpty ? "iphone.slash" : "iphone").frame(width: 20).foregroundStyle(.secondary)
            if store.devices.isEmpty {
                VStack(alignment: .leading, spacing: 2) {
                    Text("No device connected").font(.callout.weight(.medium))
                    Text("Unlock your iPhone on the same Wi-Fi, or plug it in.").font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                Button("Look Again") { Task { await store.refreshDevices() } }.controlSize(.small)
            } else if store.devices.count == 1, let d = store.devices.first {
                Text("Install on **\(d.name)**").font(.callout)
                Text("· \(d.connectionLabel)").font(.callout).foregroundStyle(.secondary)
                Spacer()
            } else {
                Text("Install on").font(.callout)
                Picker("", selection: $deviceUdid) {
                    ForEach(store.devices) { d in Text("\(d.name) · \(d.connectionLabel)").tag(d.udid) }
                }
                .labelsHidden().fixedSize()
                Spacer()
            }
        }
    }

    /// One App ID this install needs.
    private struct NeededId {
        var label: String
        var isExtension: Bool
        /// The spellings it may be registered under: the main app as
        /// "<id>.<TEAM>", extensions as "<id>.<TEAM>.<suffix>" (how the signer
        /// registers them).
        var forms: [String]
    }

    private func neededIdentifiers(_ i: IpaInfo) -> [NeededId] {
        let team = store.account?.teamId ?? ""
        let base = options.bundleId.flatMap { $0.isEmpty ? nil : $0 } ?? i.bundleId
        var ids = [NeededId(label: options.displayName ?? i.name, isExtension: false, forms: [base, "\(base).\(team)"])]
        if options.extensions == .registerAll {
            for ext in i.subBundles where ext.kind == "extension" {
                let suffix = ext.bundleId.hasPrefix(i.bundleId + ".")
                    ? String(ext.bundleId.dropFirst(i.bundleId.count))
                    : "." + (ext.bundleId.split(separator: ".").last.map(String.init) ?? "ext")
                ids.append(NeededId(label: ExtensionName.friendly(ext.bundleId), isExtension: true,
                                    forms: [base + suffix, "\(base).\(team)\(suffix)", "\(base)\(suffix).\(team)"]))
            }
        }
        return ids
    }

    /// "widget" when every extension is a widget, else "extension".
    private func extensionNoun(_ labels: [String], plural: Bool) -> String {
        let widgets = !labels.isEmpty && labels.allSatisfy { $0.lowercased().contains("widget") }
        return (widgets ? "widget" : "extension") + (plural ? "s" : "")
    }

    /// Free accounts: exactly which App IDs this install uses, naming each
    /// widget/extension, before anything is spent.
    @ViewBuilder private var budgetRow: some View {
        if let i = inspect?.info, let avail = store.appIds?.availableQuantity, store.account?.isFree != false {
            let registered = Set(store.appIds?.appIds.map(\.identifier) ?? [])
            // An ID counts as existing only on an exact match of one of its spellings.
            let missing = neededIdentifiers(i).filter { n in !n.forms.contains(where: registered.contains) }
            let missingExt = missing.filter(\.isExtension)
            let allExt = i.subBundles.filter { $0.kind == "extension" }.map { ExtensionName.friendly($0.bundleId) }
            let short = missing.count > avail
            let appName = options.displayName ?? i.name
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Image(systemName: short ? "exclamationmark.circle.fill" : missing.isEmpty ? "checkmark.circle" : "number.circle")
                        .frame(width: 20)
                        .foregroundStyle(short ? .orange : missing.isEmpty ? .green : .secondary)
                    VStack(alignment: .leading, spacing: 6) {
                        Text(missing.isEmpty ? "Already registered, so it uses none of this week's App IDs."
                             : "Uses \(missing.count) of your \(avail) App IDs left this week\(short ? ": not enough" : "")")
                            .font(.callout.weight(missing.isEmpty ? .regular : .medium))
                            .foregroundStyle(short ? .orange : .primary)
                        if missing.count > 1 || !missingExt.isEmpty {
                            VStack(alignment: .leading, spacing: 2) {
                                ForEach(Array(missing.enumerated()), id: \.offset) { _, n in
                                    Text(n.isExtension ? "•  \(n.label), part of \(appName)" : "•  \(n.label)")
                                }
                            }
                            .font(.callout).foregroundStyle(.secondary)
                        }
                        if !missingExt.isEmpty {
                            let noun = extensionNoun(missingExt.map(\.label), plural: missingExt.count > 1)
                            Text("On a free Apple ID every \(extensionNoun(missingExt.map(\.label), plural: false)) needs its own App ID, and you get 10 a week. Leave the \(noun) out to use \(missing.count - missingExt.count == 0 ? "none" : "just 1"); \(appName) still works, without the \(noun).")
                                .font(.caption).foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                            Button("Install Without \(noun.capitalized)") { options.extensions = .strip }
                                .controlSize(.small)
                        } else if options.extensions == .strip, !allExt.isEmpty {
                            let noun = extensionNoun(allExt, plural: allExt.count > 1)
                            Text("Without its \(noun): \(allExt.joined(separator: ", ")).")
                                .font(.caption).foregroundStyle(.secondary)
                            Button("Include \(noun.capitalized)") { options.extensions = .registerAll }
                                .controlSize(.small)
                        } else if options.extensions == .reuseMain, !allExt.isEmpty {
                            Text("Its \(extensionNoun(allExt, plural: allExt.count > 1)) share \(appName)'s App ID, so some may not work.")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
            }
            .help("Free Apple IDs get 10 App IDs per 7 days. Every new app uses one, and so does each of its widgets and other extensions. Refreshing an installed app uses none.")
        }
    }

    private var keyConsent: some View {
        VStack(alignment: .leading, spacing: 4) {
            Toggle("Allow this app to receive my signing key", isOn: $options.allowSigningKey)
                .font(.callout.weight(.medium))
            Text("AltStore, SideStore and LiveContainer refresh themselves with your certificate and its private key, so it gets copied into the app. Only allow it for a copy from the official source, and never share the signed IPA.")
                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
        .padding(12)
        .background(Color.yellow.opacity(0.08), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private var optionsDisclosure: some View {
        DisclosureGroup(isExpanded: $showOptions) {
            VStack(alignment: .leading, spacing: 14) {
                Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 12, verticalSpacing: 10) {
                    GridRow {
                        Text("Name").foregroundStyle(.secondary)
                        TextField(inspect?.info.name ?? "", text: Binding(get: { options.displayName ?? "" }, set: { options.displayName = $0.isEmpty ? nil : $0 }))
                            .textFieldStyle(.roundedBorder)
                    }
                    GridRow {
                        Text("Bundle ID").foregroundStyle(.secondary)
                        TextField(inspect?.info.bundleId ?? "", text: Binding(get: { options.bundleId ?? "" }, set: { options.bundleId = $0.isEmpty ? nil : $0 }))
                            .textFieldStyle(.roundedBorder)
                    }
                    if (inspect?.info.extensionCount ?? 0) > 0 {
                        GridRow(alignment: .top) {
                            Text("Extensions").foregroundStyle(.secondary)
                            VStack(alignment: .leading, spacing: 4) {
                                Picker("", selection: $options.extensions) {
                                    ForEach(ExtensionsMode.allCases) { Text($0.label).tag($0) }
                                }
                                .pickerStyle(.segmented).labelsHidden()
                                Text(options.extensions.explanation).font(.caption).foregroundStyle(.secondary)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }
                }
                VStack(alignment: .leading, spacing: 6) {
                    Toggle("Refresh automatically before it expires", isOn: $options.autoRefresh)
                    Toggle("Allow on any device model", isOn: $options.removeSupportedDevices)
                    if inspect?.info.hasWatchApp == true {
                        Toggle("Remove the Apple Watch app", isOn: $options.stripWatchApp)
                    }
                    Toggle("Show the app's files in Files and Finder", isOn: $options.fileSharing)
                    Toggle("Request increased memory limit", isOn: $options.increasedMemoryLimit)
                    if inspect?.info.encrypted == true {
                        Toggle("Install anyway even though it's encrypted", isOn: $options.allowEncrypted)
                    }
                }
                tweaks
            }
            .padding(.top, 10)
            .font(.callout)
        } label: {
            Text(optionsSummary).font(.callout).foregroundStyle(.secondary)
                .contentShape(Rectangle())
                .onTapGesture { withAnimation { showOptions.toggle() } }
        }
    }

    private var optionsSummary: String {
        var changed: [String] = []
        if options.displayName != nil { changed.append("name") }
        if options.bundleId != nil { changed.append("bundle ID") }
        if options.extensions != .registerAll { changed.append("extensions") }
        if !options.inject.isEmpty { changed.append("\(options.inject.count) tweak\(options.inject.count == 1 ? "" : "s")") }
        if !options.autoRefresh { changed.append("no auto-refresh") }
        return changed.isEmpty ? "Options" : "Options · " + changed.joined(separator: ", ")
    }

    private var tweaks: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Tweaks").foregroundStyle(.secondary)
                Spacer()
                Button("Add…", action: addInjection).controlSize(.small)
            }
            if options.inject.isEmpty {
                Text("Drop .dylib, .deb, .framework or .bundle files here. Substrate tweaks also need ElleKit (libellekit.dylib).")
                    .font(.caption).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
            }
            ForEach(options.inject, id: \.self) { p in
                HStack {
                    Image(systemName: p.hasSuffix(".deb") ? "shippingbox" : "puzzlepiece.extension").foregroundStyle(.secondary)
                    Text(URL(fileURLWithPath: p).lastPathComponent)
                    Spacer()
                    Button { options.inject.removeAll { $0 == p }; Store.unstage(p) } label: { Image(systemName: "minus.circle") }.buttonStyle(.plain)
                }
            }
        }
    }

    // MARK: Progress / result

    @ViewBuilder private func progress(_ job: JobInfo) -> some View {
        let name = inspect?.info.name ?? source.deletingPathExtension().lastPathComponent
        VStack(spacing: 16) {
            ZStack(alignment: .bottomTrailing) {
                AppIconView(path: staged, name: name, size: 80)
                if job.status == .succeeded {
                    Image(systemName: "checkmark.circle.fill")
                        .font(.system(size: 28)).foregroundStyle(.white, .green)
                        .offset(x: 8, y: 8)
                        .transition(.scale.combined(with: .opacity))
                }
            }
            .padding(.top, 6)
            switch job.status {
            case .queued, .running:
                Text(job.kind == .export ? "Signing \(name)…" : "Installing \(name)…").font(.title3.weight(.semibold))
                JobProgressView(job: job).frame(width: 320)
            case .succeeded:
                Text(job.kind == .export ? "Signed IPA saved" : "\(name) is installed").font(.title3.weight(.semibold))
                Text(successNote(job)).font(.callout).foregroundStyle(.secondary)
                    .multilineTextAlignment(.center).frame(maxWidth: 400)
                    .fixedSize(horizontal: false, vertical: true)
            case .cancelled:
                Text("Cancelled").foregroundStyle(.secondary)
            case .failed:
                EmptyView()
            }
            ForEach(job.warnings, id: \.self) { w in
                Label(w, systemImage: "exclamationmark.circle").font(.caption).foregroundStyle(.orange)
            }
        }
        .frame(maxWidth: .infinity)
        .animation(.spring(duration: 0.35), value: job.status)
    }

    private func successNote(_ job: JobInfo) -> String {
        if job.kind == .export { return "Saved where you chose." }
        if let dev = store.devices.first(where: { $0.udid == job.deviceUdid }), dev.developerMode == false {
            return "Before opening it, turn on Developer Mode on \(dev.name): Settings → Privacy & Security → Developer Mode, then restart."
        }
        var note = "Loadside will keep it signed in the background."
        note += " If iOS says \"Untrusted Developer\", go to Settings → General → VPN & Device Management and trust your Apple ID."
        return note
    }

    // MARK: Footer

    private var footer: some View {
        HStack {
            if let job, job.status == .succeeded {
                if let out = store.exportDestinations[job.id] ?? job.outputPath.map({ URL(fileURLWithPath: $0) }), job.kind == .export {
                    Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([out]) }
                }
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.defaultAction).primaryButton()
            } else if let job, job.isActive {
                Text("You can close this; it keeps going in the background.").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Hide") { dismiss() }
            } else {
                Menu {
                    Button("Export Signed IPA…") { Task { await export() } }.disabled(!canSign || busy)
                    Button("Choose Different File…", action: chooseOther)
                } label: { Image(systemName: "ellipsis") }
                    .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
                    .disabled(inspect == nil)
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button {
                    Task { await install() }
                } label: {
                    Text(job?.status == .failed ? "Try Again" : "Install").frame(minWidth: 80)
                }
                .keyboardShortcut(.defaultAction)
                .primaryButton()
                .disabled(!canSign || deviceUdid.isEmpty || busy)
            }
        }
        .controlSize(.large)
    }

    // MARK: Logic

    private var needsKeyConsent: Bool {
        let id = options.bundleId.flatMap { $0.isEmpty ? nil : $0 } ?? inspect?.info.bundleId ?? ""
        return Self.keyReceivers.contains(id) || job?.error?.code == "signing_key_requested"
    }

    private var canSign: Bool {
        guard let i = inspect?.info else { return false }
        return store.account != nil && (i.encrypted != true || options.allowEncrypted) && !(job?.isActive ?? false)
    }

    private func load() async {
        do {
            // Hand the daemon a copy inside Loadside's own folder.
            let s = try await Store.stage(source)
            staged = s.path
            inspect = try await store.inspect(path: s.path, options: options)
            if store.account?.isFree == true && store.appIds == nil { await store.loadAppIds() }
        } catch {
            inspectError = (error as? Failure) ?? Failure.local(error.localizedDescription)
        }
    }

    /// Drop the staged copy unless the daemon still needs it: a running job
    /// reads it, and without a library copy the app record points at it.
    private func cleanUp() {
        if let job, job.isActive { return }
        if job?.status == .succeeded, job?.kind == .install, store.config?.keepLibraryCopy != true { return }
        if let staged { Store.unstage(staged) }
        for p in options.inject { Store.unstage(p) }
    }

    private func chooseOther() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [ContentView.ipaType, .applicationBundle]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        dismiss()
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { store.pendingInstallURL = url }
    }

    private func addInjection() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [UTType(filenameExtension: "dylib"), UTType(filenameExtension: "deb"), .bundle, .framework].compactMap { $0 }
        panel.canChooseDirectories = true
        panel.treatsFilePackagesAsDirectories = false
        panel.allowsMultipleSelection = true
        if panel.runModal() == .OK { for u in panel.urls { addTweak(u) } }
    }

    private func addTweak(_ url: URL) {
        Task {
            do {
                let s = try await Store.stage(url)
                if !options.inject.contains(where: { URL(fileURLWithPath: $0).lastPathComponent == url.lastPathComponent }) {
                    options.inject.append(s.path)
                    showOptions = true
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
                    if ["dylib", "deb", "framework", "bundle"].contains(url.pathExtension.lowercased()) {
                        addTweak(url)
                    } else {
                        store.show(Failure.local("Drop tweaks here", hint: ".dylib, .deb, .framework or .bundle files. To install a different app, close this first."))
                    }
                }
            }
        }
        return true
    }

    private func install() async {
        guard let staged else { return }
        busy = true
        defer { busy = false }
        jobId = await store.run {
            try await store.install(InstallParams(path: staged, deviceUdid: deviceUdid, options: options, exportPath: nil, install: true))
        }
    }

    private func export() async {
        guard let staged, let info = inspect?.info, !busy else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [ContentView.ipaType]
        panel.nameFieldStringValue = "\(info.name)-signed.ipa"
        guard panel.runModal() == .OK, let out = panel.url else { return }
        busy = true
        defer { busy = false }
        // The daemon writes into Loadside's folder; the app moves it to `out`.
        guard let staging = try? Store.exportStaging(name: out.lastPathComponent) else { return }
        jobId = await store.run {
            try await store.install(InstallParams(path: staged, deviceUdid: deviceUdid.isEmpty ? nil : deviceUdid, options: options, exportPath: staging.path, install: false))
        }
        if let jobId { store.exportDestinations[jobId] = out }
    }
}

// MARK: - Prompts inside sheets

extension View {
    /// A sheet hides the main window's prompt sheet, so it shows 2FA and
    /// certificate prompts itself.
    func presentsPrompts() -> some View { modifier(PromptPresenter()) }
}

private struct PromptPresenter: ViewModifier {
    @Environment(Store.self) private var store
    func body(content: Content) -> some View {
        content.sheet(item: Binding(get: { store.prompts.first }, set: { _ in })) { prompt in
            PromptSheet(prompt: prompt).environment(store).interactiveDismissDisabled()
        }
    }
}
