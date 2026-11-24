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
    var appIdGroups: [AppIdGroup] {
        guard let list = appIds?.appIds else { return [] }
        let team = account?.teamId ?? ""
        // Spellings differ by signer ("<id>.<TEAM>.<ext>", "<id>.<ext>.<TEAM>");
        // compare with the team component taken out.
        func norm(_ id: String) -> String {
            team.isEmpty ? id : id.split(separator: ".").filter { $0 != team }.joined(separator: ".")
        }
        let sorted = list.sorted { norm($0.identifier).count < norm($1.identifier).count }
        var groups: [AppIdGroup] = []
        for a in sorted {
            let n = norm(a.identifier)
            if let gi = groups.lastIndex(where: { n.hasPrefix(norm($0.main.identifier) + ".") }) {
                groups[gi].extensions.append(a)
            } else {
                let app = apps.first { norm($0.bundleId) == n || norm($0.originalBundleId) == n }
                groups.append(AppIdGroup(main: a, extensions: [], tracked: app))
            }
        }
        return groups.sorted { ($0.main.expiresAt ?? .max) < ($1.main.expiresAt ?? .max) }
    }

    /// Apps registered on this Apple ID (by another tool, or removed from
    /// Loadside) that Loadside isn't refreshing. They still take a device slot.
    var untrackedAppIdGroups: [AppIdGroup] {
        guard account?.isFree == true else { return [] }  // paid App IDs are permanent; too noisy
        let now = Int(Date().timeIntervalSince1970)
        return appIdGroups.filter { $0.tracked == nil && ($0.main.expiresAt ?? .max) > now }
    }

    /// Free app slots on the device. Apps installed with other tools on the
    /// same Apple ID count too (assumed still on the device).
    var freeSlotsLeft: Int? {
        slotsUsed.map { max(0, Store.freeSlotsPerDevice - $0.used - untrackedAppIdGroups.count) }
    }
}
