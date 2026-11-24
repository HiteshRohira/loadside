import Foundation

/// Starts Loadside in the menu bar at login whenever the menu bar icon is on,
/// so a background service that refreshes apps is never invisible. Turning the
/// icon off removes the login item (the service keeps running either way).
///
/// A per-user LaunchAgent, like the daemon's: it runs this app's own signed
/// binary (so Login Items lists it as Loadside) with `--login`, which starts
/// the app without a window or Dock icon. No KeepAlive: quitting is respected.
enum LoginItem {
    static let label = "dev.loadside.menubar"
    static let launchArgument = "--login"

    private static var plistURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/\(label).plist")
    }

    /// Match the login item to the menu bar setting. Cheap; call at launch and
    /// whenever the setting changes. Rewrites the plist if the app moved.
    static func sync(enabled: Bool) {
        let url = plistURL
        guard enabled else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        // Only installed copies register; a build run from dist/ or Xcode's
        // DerivedData shouldn't take over the login item.
        guard let exe = Bundle.main.executablePath, Bundle.main.bundlePath.hasSuffix("/Applications/Loadside.app") else { return }
        let plist: [String: Any] = [
            "Label": label,
            "ProgramArguments": [exe, launchArgument],
            "RunAtLoad": true,
            "LimitLoadToSessionType": "Aqua",
            "ProcessType": "Interactive",
            "AssociatedBundleIdentifiers": [Bundle.main.bundleIdentifier ?? "dev.loadside.app"],
        ]
        guard let data = try? PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0) else { return }
        if (try? Data(contentsOf: url)) == data { return }
        try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? data.write(to: url, options: .atomic)
    }
}
