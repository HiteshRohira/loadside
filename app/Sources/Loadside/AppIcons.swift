import AppKit
import ImageIO
import Observation

/// Real app icons, pulled out of IPAs and cached on disk.
///
/// Xcode-built IPAs ship `AppIcon60x60@2x.png`-style files in the app bundle
/// root (Apple's "CgBI" PNG variant, which ImageIO decodes). Apps whose icon
/// only lives in Assets.car fall back to a letter tile.
///
/// IPAs are untrusted: everything read from them is size- and time-bounded,
/// links and special files are refused, and images are decoded as small
/// thumbnails only after their pixel size has been checked.
@MainActor
@Observable
final class AppIcons {
    static let shared = AppIcons()

    /// Keyed by path + file stamp, so a replaced IPA gets its new icon.
    private(set) var images: [String: NSImage] = [:]
    /// Failed loads are retried after a while (or as soon as the file changes).
    @ObservationIgnored private var attempted: [String: Date] = [:]
    @ObservationIgnored private var lastByPath: [String: String] = [:]

    /// The icon for an IPA (or .app) path, loading it in the background on first use.
    func image(for path: String) -> NSImage? {
        let stamp = Self.stamp(path)
        // A staged copy deleted after install keeps showing the icon it had.
        if stamp == "-" { return lastByPath[path].flatMap { images[$0] } }
        let key = "\(path)|\(stamp)"
        if let img = images[key] { lastByPath[path] = key; return img }
        if let t = attempted[key], Date().timeIntervalSince(t) < 300 { return nil }
        attempted[key] = Date()
        Task {
            if let img = await Self.load(path) { images[key] = img }
        }
        return nil
    }

    private nonisolated static func stamp(_ path: String) -> String {
        guard let a = try? FileManager.default.attributesOfItem(atPath: path) else { return "-" }
        let m = (a[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0
        let s = (a[.size] as? NSNumber)?.int64Value ?? 0
        return "\(m)|\(s)"
    }

    private nonisolated static var cacheDir: URL {
        FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Loadside/Icons", isDirectory: true)
    }

    /// Largest icon file we read, largest listing we parse, longest unzip run.
    private nonisolated static let maxIconBytes = 2 * 1024 * 1024
    private nonisolated static let maxListingBytes = 16 * 1024 * 1024
    private nonisolated static let maxPixels = 4096
    private nonisolated static let timeout: TimeInterval = 10

    private nonisolated static func load(_ path: String) async -> NSImage? {
        await Task.detached(priority: .utility) { () -> NSImage? in
            let url = URL(fileURLWithPath: path)
            // FNV-1a (String.hashValue changes every launch).
            var h: UInt64 = 0xcbf2_9ce4_8422_2325
            for b in (path + "|" + stamp(path)).utf8 { h = (h ^ UInt64(b)) &* 0x100_0000_01b3 }
            let cached = cacheDir.appendingPathComponent("\(String(h, radix: 16)).png")
            if let img = thumbnail(try? Data(contentsOf: cached)) { return img }

            let data = url.pathExtension.lowercased() == "app" ? bestIcon(inAppDir: url) : bestIcon(inIPA: url)
            guard let img = thumbnail(data) else { return nil }
            // Store a plain PNG so later loads skip unzip.
            if let tiff = img.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
               let png = rep.representation(using: .png, properties: [:]) {
                try? FileManager.default.createDirectory(at: cacheDir, withIntermediateDirectories: true)
                try? png.write(to: cached)
            }
            return img
        }.value
    }

    /// Decode at most a 256 px thumbnail, refusing images with huge pixel sizes.
    private nonisolated static func thumbnail(_ data: Data?) -> NSImage? {
        guard let data, !data.isEmpty, data.count <= maxIconBytes,
              let src = CGImageSourceCreateWithData(data as CFData, nil),
              let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any],
              let w = props[kCGImagePropertyPixelWidth] as? Int, let hgt = props[kCGImagePropertyPixelHeight] as? Int,
              w > 0, hgt > 0, w <= maxPixels, hgt <= maxPixels
        else { return nil }
        let opts: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceThumbnailMaxPixelSize: 256,
            kCGImageSourceShouldCacheImmediately: true,
        ]
        guard let cg = CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary) else { return nil }
        return NSImage(cgImage: cg, size: NSSize(width: cg.width, height: cg.height))
    }

    /// Bigger is better; iPhone icons beat iPad ones at the same size.
    /// Numbers come from untrusted file names, so the math is clamped.
    nonisolated static func score(_ name: String) -> Int {
        let lower = name.lowercased()
        guard lower.hasSuffix(".png"), lower.contains("icon") else { return -1 }
        var s = 1.0
        if let r = lower.range(of: #"(\d{1,6}(\.\d{1,3})?)x\d{1,6}(\.\d{1,3})?"#, options: .regularExpression) {
            s = Double(lower[r].split(separator: "x").first ?? "1") ?? 1
        }
        if let r = lower.range(of: #"@(\d)x"#, options: .regularExpression) {
            s *= Double(lower[r].dropFirst().dropLast()) ?? 1
        }
        if lower.contains("~ipad") { s -= 1 }
        if lower.contains("appicon") { s += 1_000_000 }
        return Int(min(max(s, 0), 10_000_000))
    }

    private nonisolated static func bestIcon(inIPA ipa: URL) -> Data? {
        // unzip treats the archive path as a wildcard pattern; give it a
        // name without special characters.
        var archive = ipa
        var temp: URL?
        if ipa.path.contains(where: { "[]*?\\".contains($0) }) {
            let link = FileManager.default.temporaryDirectory.appendingPathComponent("loadside-icon-\(UUID().uuidString).ipa")
            guard (try? FileManager.default.createSymbolicLink(at: link, withDestinationURL: ipa)) != nil else { return nil }
            archive = link
            temp = link
        }
        defer { if let temp { try? FileManager.default.removeItem(at: temp) } }

        guard let listing = run(["-Z1", archive.path], limit: maxListingBytes) else { return nil }
        let names = String(decoding: listing, as: UTF8.self).split(separator: "\n").map(String.init)
        let pattern = #"^Payload/[^/]+\.app/[^/]+\.png$"#
        let best = names
            .filter { $0.range(of: pattern, options: .regularExpression) != nil }
            .map { ($0, score(($0 as NSString).lastPathComponent)) }
            .filter { $0.1 >= 0 }
            .max { $0.1 < $1.1 }?.0
        guard let best else { return nil }
        // Member names are wildcard patterns too; escape the special ones.
        var escaped = ""
        for c in best {
            if "[]*?\\".contains(c) { escaped.append("\\") }
            escaped.append(c)
        }
        return run(["-p", archive.path, escaped], limit: maxIconBytes).flatMap { $0.isEmpty ? nil : $0 }
    }

    private nonisolated static func bestIcon(inAppDir app: URL) -> Data? {
        // The bundle itself must be a real folder, not a link to one.
        guard let root = try? FileManager.default.attributesOfItem(atPath: app.path),
              root[.type] as? FileAttributeType == .typeDirectory,
              let items = try? FileManager.default.contentsOfDirectory(atPath: app.path) else { return nil }
        let best = items.map { ($0, score($0)) }.filter { $0.1 >= 0 }.max { $0.1 < $1.1 }?.0
        guard let best else { return nil }
        // Open without following links, then check what was actually opened
        // (no check-then-open race): a regular file, not too big.
        let fd = open(app.appendingPathComponent(best).path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)
        guard fd >= 0 else { return nil }
        let fh = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        var st = stat()
        guard fstat(fd, &st) == 0, (st.st_mode & S_IFMT) == S_IFREG, st.st_size <= maxIconBytes else { return nil }
        return try? fh.read(upToCount: maxIconBytes)
    }

    /// Run unzip, keeping at most `limit` bytes of output and giving up after
    /// `timeout` seconds. Returns nil on failure or when the limit is exceeded.
    private nonisolated static func run(_ args: [String], limit: Int) -> Data? {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/unzip")
        // zipinfo mode (-Z) must come first; -qq would break it.
        p.arguments = args.first == "-Z1" ? args : ["-qq"] + args
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        do { try p.run() } catch { return nil }
        let killer = DispatchWorkItem { if p.isRunning { p.terminate() } }
        DispatchQueue.global().asyncAfter(deadline: .now() + timeout, execute: killer)
        defer { killer.cancel() }
        var data = Data()
        let fh = out.fileHandleForReading
        while true {
            guard let chunk = try? fh.read(upToCount: 64 * 1024), !chunk.isEmpty else { break }
            data.append(chunk)
            if data.count > limit {
                p.terminate()
                p.waitUntilExit()
                return nil
            }
        }
        p.waitUntilExit()
        return p.terminationStatus == 0 && p.terminationReason == .exit ? data : nil
    }
}
