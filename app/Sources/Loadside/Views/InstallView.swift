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
    /// The device the user had chosen, if it went away (never switch silently).
    @State private var lostDevice: (udid: String, name: String)?

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
        // Retries once the background service connects (opening an IPA can
        // launch Loadside before it's up).
        .task(id: store.connection == .connected) { if inspect == nil { await load() } }
        .onAppear { if deviceUdid.isEmpty { deviceUdid = store.devices.first?.udid ?? "" } }
        .onChange(of: store.devices) { old, devs in
            if let lost = lostDevice, devs.contains(where: { $0.udid == lost.udid }) {
                // It came back.
                deviceUdid = lost.udid
                lostDevice = nil
            } else if !deviceUdid.isEmpty, !devs.contains(where: { $0.udid == deviceUdid }) {
                // The chosen device went away: ask again instead of quietly
                // installing on another one.
                lostDevice = (deviceUdid, old.first { $0.udid == deviceUdid }?.name ?? "The device")
                deviceUdid = ""
            } else if deviceUdid.isEmpty, lostDevice == nil, let first = devs.first {
                deviceUdid = first.udid
            }
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
                slotsRow
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
            Image(systemName: store.devices.isEmpty || deviceUdid.isEmpty ? "iphone.slash" : "iphone").frame(width: 20).foregroundStyle(.secondary)
            if let lost = lostDevice, deviceUdid.isEmpty {
                VStack(alignment: .leading, spacing: 2) {
                    Text("\(lost.name) disconnected").font(.callout.weight(.medium))
                    Text(store.devices.isEmpty ? "Unlock it on the same Wi-Fi, or plug it in." : "Wait for it, or choose another device.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                if store.devices.isEmpty {
                    Button("Look Again") { Task { await store.refreshDevices() } }.controlSize(.small)
                } else {
                    Menu("Choose…") {
                        ForEach(store.devices) { d in
                            Button("\(d.name) · \(d.connectionLabel)") { deviceUdid = d.udid; lostDevice = nil }
                        }
                    }
                    .fixedSize()
                }
            } else if store.devices.isEmpty {
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
        /// Exactly what the signer registers: "<id>.<TEAM>" for the app and
        /// "<id>.<TEAM>.<rest>" for each extension (isideload), after
        /// Loadside's own extension renaming (ipa.rs `prepare`).
        var identifier: String
    }

    private func neededIdentifiers(_ i: IpaInfo) -> [NeededId] {
        let team = store.account?.teamId ?? ""
        let custom = options.bundleId?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let base = custom.isEmpty ? i.bundleId : custom
        let registered = { (id: String) in "\(base).\(team)" + id.dropFirst(base.count) }
        var ids = [NeededId(label: options.displayName ?? i.name, isExtension: false, identifier: "\(base).\(team)")]
        guard options.extensions == .registerAll else { return ids }
        var used = [base]
        for ext in i.subBundles where ext.kind == "extension" {
            var fixed: String
            if ext.bundleId.hasPrefix(i.bundleId + ".") {
                fixed = base + ext.bundleId.dropFirst(i.bundleId.count)
            } else if ext.bundleId.hasPrefix(base + ".") {
                fixed = ext.bundleId
            } else {
                fixed = base + "." + (ext.bundleId.split(separator: ".").last.map(String.init) ?? "ext")
            }
            if used.contains(fixed) {
                let stem = fixed
                var n = 2
                while used.contains(fixed) { fixed = "\(stem)\(n)"; n += 1 }
            }
            used.append(fixed)
            ids.append(NeededId(label: ExtensionName.friendly(ext.bundleId), isExtension: true, identifier: registered(fixed)))
        }
        return ids
    }

    /// Free accounts keep 3 sideloaded apps per device. Installing a 4th
    /// would spend App IDs and then fail on the device, so stop it here.
    /// (Updating an app that's already there doesn't take a new slot.)
    private var slotsFull: Bool {
        guard store.account?.isFree == true, !deviceUdid.isEmpty, let i = inspect?.info else { return false }
        // An update keeps its final bundle ID (custom ID or the app's, plus the team).
        let custom = options.bundleId?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let finalId = "\(custom.isEmpty ? i.bundleId : custom).\(store.account?.teamId ?? "")"
        let others = store.activeApps(on: deviceUdid).filter { $0.bundleId != finalId }
        return others.count >= Store.freeSlotsPerDevice
    }

    @ViewBuilder private var slotsRow: some View {
        if slotsFull {
            let name = store.devices.first { $0.udid == deviceUdid }?.name ?? "This device"
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: "exclamationmark.circle.fill").frame(width: 20).foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 2) {
                    Text("\(name) already has 3 apps from Loadside").font(.callout.weight(.medium)).foregroundStyle(.orange)
                    Text("Free Apple IDs allow 3 sideloaded apps per device. Remove one first (open it on the home screen, then Remove).")
                        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
        }
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
            let missing = neededIdentifiers(i).filter { !registered.contains($0.identifier) }
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
                        // The signer can't sign Watch apps, so they're always removed.
                        Label("The Apple Watch app is left out (it can't be signed).", systemImage: "applewatch.slash")
                            .font(.callout).foregroundStyle(.secondary)
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
                Text(job.kind == .export ? (store.exportErrors[job.id] == nil ? "Signed IPA saved" : "Couldn't save the signed IPA") : "\(name) is installed").font(.title3.weight(.semibold))
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
        if job.kind == .export { return store.exportErrors[job.id] ?? "Saved where you chose." }
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
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction).disabled(busy)
                Button {
                    Task { await install() }
                } label: {
                    Text(job?.status == .failed ? "Try Again" : "Install").frame(minWidth: 80)
                }
                .keyboardShortcut(.defaultAction)
                .primaryButton()
                .disabled(!canSign || deviceUdid.isEmpty || busy || slotsFull)
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
            // Opening an IPA can launch Loadside before the background
            // service is up; this runs again once it connects.
            guard store.connection == .connected else { return }
            inspectError = nil
            // Hand the daemon a copy inside Loadside's own folder (once).
            let path: String
            if let staged { path = staged } else {
                path = try await Store.stage(source).path
                // Closed while copying: drop the copy instead of leaking it.
                if Task.isCancelled {
                    Store.unstage(path)
                    return
                }
                staged = path
                store.liveStaging.insert(path)
            }
            inspect = try await store.inspect(path: path, options: options)
            // Always fresh: App IDs expire, and another tool may have added some.
            if store.account?.isFree == true { await store.loadAppIds() }
        } catch {
            inspectError = (error as? Failure) ?? Failure.local(error.localizedDescription)
        }
    }

    /// Drop the staged copies unless something still needs them: a job that
    /// is running (or being submitted) drops them itself when it ends, and an
    /// installed app may refresh from them (Store.unstageIfUnused checks).
    private func cleanUp() {
        if let staged { store.liveStaging.remove(staged) }
        if busy || (job?.isActive ?? false) { return }
        store.forgetStaged(job: jobId)
        for p in [staged].compactMap({ $0 }) + options.inject { store.unstageIfUnused(p) }
    }

    private var stagedInputs: [String] { [staged].compactMap { $0 } + options.inject }

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
        if options.inject.contains(where: { URL(fileURLWithPath: $0).lastPathComponent == url.lastPathComponent }) { return }
        Task {
            do {
                let s = try await Store.stage(url)
                if options.inject.contains(where: { URL(fileURLWithPath: $0).lastPathComponent == url.lastPathComponent }) {
                    Store.unstage(s.path)
                } else {
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
        // An empty target would let the daemon pick a device on its own.
        guard let staged, !deviceUdid.isEmpty else { return }
        let target = deviceUdid
        store.forgetStaged(job: jobId)
        busy = true
        defer { busy = false }
        let inputs = stagedInputs
        jobId = await store.run {
            try await store.install(InstallParams(path: staged, deviceUdid: target, options: options, exportPath: nil, install: true))
        }
        if let jobId { store.unstageWhenDone(job: jobId, paths: inputs) }
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
        let inputs = stagedInputs
        store.forgetStaged(job: jobId)
        jobId = await store.run {
            try await store.install(InstallParams(path: staged, deviceUdid: deviceUdid.isEmpty ? nil : deviceUdid, options: options, exportPath: staging.path, install: false))
        }
        if let jobId {
            store.exportDestinations[jobId] = out
            store.unstageWhenDone(job: jobId, paths: inputs)
        }
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
            PromptSheet(prompt: prompt).id(prompt.id).environment(store).interactiveDismissDisabled()
        }
    }
}
