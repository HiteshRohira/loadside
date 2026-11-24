import Foundation

/// An app's App ID with the App IDs of its widgets and other extensions, so
/// "DownloadsWidgetExtension" shows up as Nuvio's Downloads widget instead of
/// a mystery entry using up the weekly budget.
struct AppIdGroup: Identifiable {
    var main: AppIdSummary
    var extensions: [AppIdSummary]
    /// The Loadside app this App ID belongs to, if any.
    var tracked: AppRecord?
    var id: String { main.identifier }
}

enum ExtensionName {
    /// "DownloadsWidgetExtension" → "Downloads widget",
    /// "NotificationService" → "Notification service extension".
    static func friendly(_ raw: String) -> String {
        var s = raw
        if let last = s.split(separator: ".").last, s.contains(".") { s = String(last) }
        for suffix in ["Extension", "Target", "Ext"] where s.hasSuffix(suffix) && s.count > suffix.count {
            s = String(s.dropLast(suffix.count))
            break
        }
        var words: [String] = []
        var current = ""
        for ch in s {
            if ch == "_" || ch == "-" || ch == " " {
                if !current.isEmpty { words.append(current); current = "" }
            } else if ch.isUppercase, let prev = current.last, prev.isLowercase || prev.isNumber {
                words.append(current); current = String(ch)
            } else {
                current.append(ch)
            }
        }
        if !current.isEmpty { words.append(current) }
        guard let first = words.first else { return "Extension" }
        let rest = words.dropFirst().map { $0.count > 1 && $0 == $0.uppercased() ? $0 : $0.lowercased() }
        var text = ([first.prefix(1).uppercased() + first.dropFirst()] + rest).joined(separator: " ")
        let lower = text.lowercased()
        if !["widget", "extension", "keyboard", "intents", "clip"].contains(where: lower.contains) { text += " extension" }
        return text
    }
}

extension Store {
    /// App IDs on the account grouped by app (free accounts' budget view).
    ///
    /// The signer registers an app as "<id>.<TEAM>" and its extensions as
    /// "<id>.<TEAM>.<rest>", so an extension's identifier starts with its
    /// app's full identifier. A separate app such as "com.x.app.beta" is
    /// "com.x.app.beta.<TEAM>" and stays its own group.
    var appIdGroups: [AppIdGroup] {
        guard let list = appIds?.appIds else { return [] }
        let sorted = list.sorted { $0.identifier.count < $1.identifier.count }
        var groups: [AppIdGroup] = []
        for a in sorted {
            let tracked = apps.first { $0.bundleId == a.identifier }
            if tracked == nil, let gi = groups.lastIndex(where: { a.identifier.hasPrefix($0.main.identifier + ".") }) {
                groups[gi].extensions.append(a)
            } else {
                groups.append(AppIdGroup(main: a, extensions: [], tracked: tracked))
            }
        }
        return groups.sorted { ($0.main.expiresAt ?? .max) < ($1.main.expiresAt ?? .max) }
    }

    /// Free app slots on the device Loadside knows about. Apps installed by
    /// other tools can't be counted reliably (an App ID outlives the app by
    /// up to 7 days), so they aren't; the budget tooltip says so.
    var freeSlotsLeft: Int? {
        slotsUsed.map { max(0, Store.freeSlotsPerDevice - $0.used) }
    }
}
