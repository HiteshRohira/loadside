// Renders a comparison sheet of the icon styles in IconArt.swift.
// swiftc -O scripts/icon-options.swift app/Sources/Loadside/IconArt.swift -o /tmp/icon-options && /tmp/icon-options docs/icons
import AppKit

@main struct IconOptions {
    static func png(_ size: NSSize, scale: CGFloat = 2, _ draw: (CGFloat) -> Void) -> Data {
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size.width * scale), pixelsHigh: Int(size.height * scale), bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        rep.size = size
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        draw(scale)
        NSGraphicsContext.restoreGraphicsState()
        return rep.representation(using: .png, properties: [:])!
    }

    static func text(_ s: String, _ p: CGPoint, size: CGFloat, weight: NSFont.Weight = .regular, color: NSColor = .white, width: CGFloat = 400) {
        // p is the top-left corner of the text box (y-up canvas).
        (s as NSString).draw(in: NSRect(x: p.x, y: p.y - 60, width: width, height: 60),
                             withAttributes: [.font: NSFont.systemFont(ofSize: size, weight: weight), .foregroundColor: color])
    }

    static func glyph(_ style: IconArt.Style, at p: CGPoint, tint: NSColor, attention: Bool = false) {
        let img = IconArt.menuBarImage(style, attention: attention)
        let tinted = NSImage(size: img.size, flipped: false) { r in
            img.draw(in: r); tint.set(); r.fill(using: .sourceAtop); return true
        }
        tinted.draw(in: NSRect(x: p.x, y: p.y, width: 18, height: 18))
    }

    static func sym(_ name: String, at p: CGPoint, tint: NSColor) {
        guard let s = NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(.init(pointSize: 14, weight: .regular)) else { return }
        let t = NSImage(size: s.size, flipped: false) { r in s.draw(in: r); tint.set(); r.fill(using: .sourceAtop); return true }
        t.draw(at: NSPoint(x: p.x, y: p.y + (18 - s.size.height) / 2), from: .zero, operation: .sourceOver, fraction: 1)
    }

    static func menuBar(_ style: IconArt.Style, rect: NSRect, dark: Bool) {
        let bg = dark ? NSColor(white: 0.13, alpha: 1) : NSColor(white: 0.93, alpha: 1)
        let fg = dark ? NSColor.white : NSColor.black
        bg.setFill(); NSBezierPath(roundedRect: rect, xRadius: 6, yRadius: 6).fill()
        let y = rect.midY - 9
        var x = rect.minX + 14
        glyph(style, at: CGPoint(x: x, y: y), tint: fg); x += 34
        glyph(style, at: CGPoint(x: x, y: y), tint: fg, attention: true); x += 34
        for s in ["wifi", "battery.75percent", "magnifyingglass"] { sym(s, at: CGPoint(x: x, y: y), tint: fg.withAlphaComponent(0.85)); x += 34 }
    }

    static func main() {
        let out = URL(fileURLWithPath: CommandLine.arguments[1])
        let styles = IconArt.Style.allCases
        let colW: CGFloat = 360, H: CGFloat = 640
        let sheet = png(NSSize(width: colW * CGFloat(styles.count) + 40, height: H)) { _ in
            NSColor(srgbRed: 0.07, green: 0.07, blue: 0.09, alpha: 1).setFill(); NSRect(x: 0, y: 0, width: 4000, height: H).fill()
            for (i, st) in styles.enumerated() {
                let x0 = 20 + CGFloat(i) * colW + 20
                text(st.title, CGPoint(x: x0, y: H - 28), size: 20, weight: .semibold)
                text(st.blurb, CGPoint(x: x0, y: H - 60), size: 12, color: NSColor(white: 0.65, alpha: 1), width: colW - 40)
                NSGraphicsContext.current?.saveGraphicsState()
                let t = NSAffineTransform(); t.translateX(by: x0 + 10, yBy: 240); t.concat()
                IconArt.drawAppIcon(st, size: 300)
                NSGraphicsContext.current?.restoreGraphicsState()
                // Dock-size and Finder-size previews.
                for (k, sz) in [64, 32, 16].enumerated() {
                    NSGraphicsContext.current?.saveGraphicsState()
                    let t2 = NSAffineTransform(); t2.translateX(by: x0 + 10 + CGFloat(k) * 80, yBy: 160); t2.concat()
                    IconArt.drawAppIcon(st, size: CGFloat(sz))
                    NSGraphicsContext.current?.restoreGraphicsState()
                }
                menuBar(st, rect: NSRect(x: x0, y: 92, width: colW - 40, height: 40), dark: false)
                menuBar(st, rect: NSRect(x: x0, y: 40, width: colW - 40, height: 40), dark: true)
            }
        }
        try! sheet.write(to: out.appendingPathComponent("icon-options.png"))
        for st in styles {
            try! png(NSSize(width: 1024, height: 1024), scale: 1) { _ in IconArt.drawAppIcon(st, size: 1024) }
                .write(to: out.appendingPathComponent("icon-\(st.rawValue).png"))
        }
        print("wrote", out.path)
    }
}
