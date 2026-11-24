import AppKit

/// The menu bar glyph, drawn as a template image (black; macOS tints it for
/// light/dark menu bars). The app icon itself is artwork in
/// docs/icons/src/*.svg rendered to assets/AppIcon.icns.
enum IconArt {
    enum Style: String, CaseIterable {
        case halo, dropIn, stack, glass

        var title: String {
            switch self {
            case .halo: "A · Halo"
            case .dropIn: "B · Drop-in"
            case .stack: "C · Stack"
            case .glass: "D · Liquid Glass"
            }
        }

        var blurb: String {
            switch self {
            case .halo: "Your iPhone inside a glowing refresh ring: apps kept signed, always."
            case .dropIn: "An app tile sliding into an iPhone from the side. Light, friendly."
            case .stack: "Your apps as a stack of glossy tiles, the front one refreshing."
            case .glass: "A frosted-glass iPhone over an aurora, in the macOS 26 style."
            }
        }

        var svg: String {
            switch self {
            case .halo: "halo"
            case .dropIn: "dropin"
            case .stack: "stack"
            case .glass: "glass"
            }
        }
    }

    /// The style the app ships with.
    static let current: Style = .halo

    private static func rr(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat, _ r: CGFloat, _ s: CGFloat) -> NSBezierPath {
        NSBezierPath(roundedRect: NSRect(x: x * s, y: y * s, width: w * s, height: h * s), xRadius: r * s, yRadius: r * s)
    }

    /// Clear a shape out of what's been drawn (gives glyph parts a gap, like SF Symbols).
    private static func knockout(_ p: NSBezierPath, line: CGFloat? = nil) {
        NSGraphicsContext.current?.saveGraphicsState()
        NSGraphicsContext.current?.compositingOperation = .clear
        if let line { p.lineWidth = line; p.stroke() } else { p.fill() }
        NSGraphicsContext.current?.restoreGraphicsState()
    }

    static func drawMenuGlyph(_ style: Style, size s: CGFloat, attention: Bool = false) {
        NSColor.black.setStroke()
        NSColor.black.setFill()
        let lw = s * 0.085
        func phone(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) {
            let p = rr(x, y, w, h, w * 0.28, s)
            p.lineWidth = lw
            p.stroke()
        }
        switch style {
        case .halo:
            // Open ring (gap top-left) with a dot at its leading end, phone inside.
            let c = NSPoint(x: 0.5 * s, y: 0.5 * s), r = 0.42 * s
            let arc = NSBezierPath()
            arc.appendArc(withCenter: c, radius: r, startAngle: 128, endAngle: 128 - 298, clockwise: true)
            arc.lineWidth = lw
            arc.lineCapStyle = .round
            arc.stroke()
            let end = (128.0 - 298.0) * Double.pi / 180
            let dot = NSPoint(x: c.x + CGFloat(cos(end)) * r, y: c.y + CGFloat(sin(end)) * r)
            NSBezierPath(ovalIn: NSRect(x: dot.x - 0.1 * s, y: dot.y - 0.1 * s, width: 0.2 * s, height: 0.2 * s)).fill()
            phone(0.385, 0.27, 0.23, 0.46)
        case .dropIn:
            phone(0.44, 0.08, 0.44, 0.84)
            // Tile entering from the left, with a gap around it.
            let tile = rr(0.22, 0.31, 0.38, 0.38, 0.1, s)
            knockout(rr(0.17, 0.26, 0.48, 0.48, 0.14, s))
            tile.fill()
            for (y, x0) in [(0.62, 0.02), (0.38, 0.06)] as [(CGFloat, CGFloat)] {
                let l = NSBezierPath()
                l.move(to: NSPoint(x: x0 * s, y: y * s)); l.line(to: NSPoint(x: 0.14 * s, y: y * s))
                l.lineWidth = lw; l.lineCapStyle = .round; l.stroke()
            }
        case .stack:
            let back = rr(0.34, 0.34, 0.56, 0.56, 0.16, s)
            back.lineWidth = lw
            back.stroke()
            knockout(rr(0.04, 0.04, 0.68, 0.68, 0.2, s))
            let front = rr(0.10, 0.10, 0.56, 0.56, 0.16, s)
            front.fill()
            // Refresh mark cut out of the front tile.
            let c = NSPoint(x: 0.38 * s, y: 0.38 * s)
            let ring = NSBezierPath()
            ring.appendArc(withCenter: c, radius: 0.14 * s, startAngle: 150, endAngle: 150 - 300, clockwise: true)
            ring.lineCapStyle = .round
            knockout(ring, line: lw * 0.95)
        case .glass:
            phone(0.24, 0.06, 0.52, 0.88)
            let a = NSBezierPath()
            a.move(to: NSPoint(x: 0.5 * s, y: 0.66 * s)); a.line(to: NSPoint(x: 0.5 * s, y: 0.32 * s))
            a.move(to: NSPoint(x: 0.37 * s, y: 0.45 * s)); a.line(to: NSPoint(x: 0.5 * s, y: 0.32 * s)); a.line(to: NSPoint(x: 0.63 * s, y: 0.45 * s))
            a.lineWidth = lw; a.lineCapStyle = .round; a.lineJoinStyle = .round
            a.stroke()
        }
        if attention {
            knockout(NSBezierPath(ovalIn: NSRect(x: 0.62 * s, y: 0.62 * s, width: 0.42 * s, height: 0.42 * s)))
            NSBezierPath(ovalIn: NSRect(x: 0.70 * s, y: 0.70 * s, width: 0.28 * s, height: 0.28 * s)).fill()
        }
    }

    /// A template NSImage of the menu bar glyph (18 pt, as macOS expects).
    static func menuBarImage(_ style: Style = current, attention: Bool = false) -> NSImage {
        let img = NSImage(size: NSSize(width: 18, height: 18), flipped: false) { rect in
            drawMenuGlyph(style, size: rect.width, attention: attention)
            return true
        }
        img.isTemplate = true
        return img
    }
}
