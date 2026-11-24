import AppKit
import Observation

/// Real app icons, pulled out of IPAs and cached on disk.
///
/// Xcode-built IPAs ship `AppIcon60x60@2x.png`-style files in the app bundle
/// root (Apple's "CgBI" PNG variant, which ImageIO decodes). Apps whose icon
/// only lives in Assets.car fall back to a letter tile.
@MainActor
@Observable
final class AppIcons {
    static let shared = AppIcons()

    private(set) var images: [String: NSImage] = [:]
    @ObservationIgnored private var attempted: Set<String> = []

    /// The icon for an IPA (or .app) path, loading it in the background on first use.
    func image(for path: String) -> NSImage? {
        if let img = images[path] { return img }
        if !attempted.contains(path) {
            attempted.insert(path)
            Task {
                if let img = await Self.load(path) { images[path] = img }
            }
        }
        return nil
    }

    private nonisolated static var cacheDir: URL {
        FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Loadside/Icons", isDirectory: true)
    }

    private nonisolated static func load(_ path: String) async -> NSImage? {
        await Task.detached(priority: .utility) { () -> NSImage? in
            let url = URL(fileURLWithPath: path)
            guard let attrs = try? FileManager.default.attributesOfItem(atPath: path) else { return nil }
            let stamp = (attrs[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0
            let size = (attrs[.size] as? NSNumber)?.intValue ?? 0
            // FNV-1a (String.hashValue changes every launch).
            var h: UInt64 = 0xcbf2_9ce4_8422_2325
            for b in (path + "|\(stamp)|\(size)").utf8 { h = (h ^ UInt64(b)) &* 0x100_0000_01b3 }
            let key = String(h, radix: 16)
            let cached = cacheDir.appendingPathComponent("\(key).png")
            if let img = NSImage(contentsOf: cached) { return img }

            let data: Data?
            if url.pathExtension.lowercased() == "app" {
                data = bestIcon(inAppDir: url)
            } else {
                data = bestIcon(inIPA: url)
            }
            guard let data, let img = NSImage(data: data), img.isValid else { return nil }
            // Store a plain PNG so later loads skip unzip.
            if let tiff = img.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
               let png = rep.representation(using: .png, properties: [:]) {
                try? FileManager.default.createDirectory(at: cacheDir, withIntermediateDirectories: true)
                try? png.write(to: cached)
            }
            return img
        }.value
    }

    /// Bigger is better; iPhone icons beat iPad ones at the same size.
    private nonisolated static func score(_ name: String) -> Int {
        let lower = name.lowercased()
        guard lower.hasSuffix(".png"), lower.contains("icon") else { return -1 }
        var s = 1
        if let r = lower.range(of: #"(\d+(\.\d+)?)x\d+(\.\d+)?"#, options: .regularExpression) {
            let dims = lower[r].split(separator: "x")
            s = Int(Double(dims.first ?? "0") ?? 0)
        }
        if let r = lower.range(of: #"@(\d)x"#, options: .regularExpression) {
            s *= Int(lower[r].dropFirst().dropLast()) ?? 1
        }
        if lower.contains("~ipad") { s -= 1 }
        if lower.contains("appicon") { s += 1000 }
        return s
    }

    private nonisolated static func bestIcon(inIPA ipa: URL) -> Data? {
        guard let listing = run(["-Z1", ipa.path]) else { return nil }
        let names = String(decoding: listing, as: UTF8.self).split(separator: "\n").map(String.init)
        let pattern = #"^Payload/[^/]+\.app/[^/]+\.png$"#
        let best = names
            .filter { $0.range(of: pattern, options: .regularExpression) != nil }
            .map { ($0, score(($0 as NSString).lastPathComponent)) }
            .filter { $0.1 >= 0 }
            .max { $0.1 < $1.1 }?.0
        guard let best else { return nil }
        // unzip treats names as wildcard patterns; escape the special ones.
        let escaped = best.replacingOccurrences(of: "[", with: "\\[")
            .replacingOccurrences(of: "*", with: "\\*")
            .replacingOccurrences(of: "?", with: "\\?")
        return run(["-p", ipa.path, escaped]).flatMap { $0.isEmpty ? nil : $0 }
    }

    private nonisolated static func bestIcon(inAppDir app: URL) -> Data? {
        guard let items = try? FileManager.default.contentsOfDirectory(atPath: app.path) else { return nil }
        let best = items.map { ($0, score($0)) }.filter { $0.1 >= 0 }.max { $0.1 < $1.1 }?.0
        return best.flatMap { try? Data(contentsOf: app.appendingPathComponent($0)) }
    }

    private nonisolated static func run(_ args: [String]) -> Data? {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/unzip")
        // zipinfo mode (-Z) must come first; -qq would break it.
        p.arguments = args.first == "-Z1" ? args : ["-qq"] + args
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        do { try p.run() } catch { return nil }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return p.terminationStatus == 0 ? data : nil
    }
}
