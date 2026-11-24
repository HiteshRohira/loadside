// Comparison sheet: rendered app icons (PNG, from docs/icons/src/*.svg) plus
// the matching menu bar glyphs from IconArt.swift on light and dark bars.
// swiftc -O -parse-as-library scripts/icon-options.swift app/Sources/Loadside/IconArt.swift -o /tmp/io && /tmp/io RENDERS_DIR OUT.png
import AppKit

@main struct IconOptions {
    static func text(_ s: String, _ p: CGPoint, size: CGFloat, weight: NSFont.Weight = .regular, color: NSColor = .white, width: CGFloat = 400) {
        (s as NSString).draw(in: NSRect(x: p.x, y: p.y - 60, width: width, height: 60), withAttributes: [.font: NSFont.systemFont(ofSize: size, weight: weight), .foregroundColor: color])
    }
    static func tinted(_ img: NSImage, _ c: NSColor) -> NSImage {
        NSImage(size: img.size, flipped: false) { r in img.draw(in: r); c.set(); r.fill(using: .sourceAtop); return true }
    }
    static func menuBar(_ st: IconArt.Style, _ rect: NSRect, dark: Bool) {
        (dark ? NSColor(white: 0.14, alpha: 1) : NSColor(white: 0.94, alpha: 1)).setFill()
        NSBezierPath(roundedRect: rect, xRadius: 6, yRadius: 6).fill()
        let fg = dark ? NSColor.white : NSColor.black
        var x = rect.minX + 16
        let y = rect.midY - 9
        for att in [false, true] { tinted(IconArt.menuBarImage(st, attention: att), fg).draw(in: NSRect(x: x, y: y, width: 18, height: 18)); x += 36 }
        for n in ["wifi", "battery.75percent", "magnifyingglass", "switch.2"] {
            if let s = NSImage(systemSymbolName: n, accessibilityDescription: nil)?.withSymbolConfiguration(.init(pointSize: 14, weight: .regular)) {
                tinted(s, fg.withAlphaComponent(0.85)).draw(at: NSPoint(x: x, y: rect.midY - s.size.height / 2), from: .zero, operation: .sourceOver, fraction: 1)
            }
            x += 34
        }
    }
    static func main() throws {
        let dir = URL(fileURLWithPath: CommandLine.arguments[1]), out = URL(fileURLWithPath: CommandLine.arguments[2])
        let styles = IconArt.Style.allCases
        let colW: CGFloat = 380, H: CGFloat = 640, W = colW * CGFloat(styles.count) + 40
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(W * 2), pixelsHigh: Int(H * 2), bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        rep.size = NSSize(width: W, height: H)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        NSColor(srgbRed: 0.075, green: 0.075, blue: 0.09, alpha: 1).setFill(); NSRect(x: 0, y: 0, width: W, height: H).fill()
        for (i, st) in styles.enumerated() {
            let x0 = 20 + CGFloat(i) * colW + 20
            text(st.title, CGPoint(x: x0, y: H - 28), size: 20, weight: .semibold)
            text(st.blurb, CGPoint(x: x0, y: H - 60), size: 12, color: NSColor(white: 0.65, alpha: 1), width: colW - 40)
            guard let icon = NSImage(contentsOf: dir.appendingPathComponent("\(st.svg).png")) else { continue }
            icon.draw(in: NSRect(x: x0, y: 220, width: 320, height: 320))
            for (k, sz) in [64, 32, 16].enumerated() {
                icon.draw(in: NSRect(x: x0 + 10 + CGFloat(k) * 84, y: 160 - CGFloat(sz) / 2 + 16, width: CGFloat(sz), height: CGFloat(sz)))
            }
            menuBar(st, NSRect(x: x0, y: 86, width: colW - 40, height: 40), dark: false)
            menuBar(st, NSRect(x: x0, y: 36, width: colW - 40, height: 40), dark: true)
        }
        NSGraphicsContext.restoreGraphicsState()
        try rep.representation(using: .png, properties: [:])!.write(to: out)
    }
}
