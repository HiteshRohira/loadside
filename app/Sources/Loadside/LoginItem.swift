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

    /// Why the last sync failed, if it did (shown in Settings → General).
    nonisolated(unsafe) static var lastError: String?

    /// Match the login item to the menu bar setting. Cheap; call at launch and
    /// whenever the setting changes. Rewrites the plist if the app moved.
    @discardableResult
    static func sync(enabled: Bool) -> Bool {
        lastError = nil
        let url = plistURL
        guard enabled else {
            do {
                if FileManager.default.fileExists(atPath: url.path) { try FileManager.default.removeItem(at: url) }
                return true
            } catch {
                lastError = "Couldn't remove the login item: \(error.localizedDescription)"
                return false
            }
        }
        // Only installed copies register; a build run from dist/ or Xcode's
        // DerivedData shouldn't take over the login item.
        guard let exe = Bundle.main.executablePath, Bundle.main.bundlePath.hasSuffix("/Applications/Loadside.app") else { return true }
        let plist: [String: Any] = [
            "Label": label,
            "ProgramArguments": [exe, launchArgument],
            "RunAtLoad": true,
            "LimitLoadToSessionType": "Aqua",
            "ProcessType": "Interactive",
            "AssociatedBundleIdentifiers": [Bundle.main.bundleIdentifier ?? "dev.loadside.app"],
        ]
        do {
            let data = try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
            if (try? Data(contentsOf: url)) == data { return true }
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try data.write(to: url, options: .atomic)
            return true
        } catch {
            lastError = "Couldn't set up starting at login: \(error.localizedDescription)"
            return false
        }
    }
}
