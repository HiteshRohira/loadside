import AppKit

/// The menu bar glyph (Sideglide): two stepped panes gliding into a rounded
/// square, matching the app icon. Drawn as a template image (black; macOS
/// tints it for light and dark menu bars). Source: docs/icons/final/sideglide-menubar.svg
/// (an 18 × 18 pt grid, y pointing down; flipped here for AppKit).
enum IconArt {
    static func drawMenuGlyph(size s: CGFloat, attention: Bool = false) {
        let k = s / 18
        func p(_ x: CGFloat, _ y: CGFloat) -> NSPoint { NSPoint(x: x * k, y: (18 - y) * k) }
        NSColor.black.setStroke()
        NSColor.black.setFill()

        let back = NSBezierPath()
        back.move(to: p(3.0, 6.55))
        back.curve(to: p(1.8, 8.2), controlPoint1: p(2.2, 6.8), controlPoint2: p(1.8, 7.4))
        back.line(to: p(1.8, 9.8))
        back.curve(to: p(3.0, 11.45), controlPoint1: p(1.8, 10.6), controlPoint2: p(2.2, 11.2))

        let middle = NSBezierPath()
        middle.move(to: p(5.85, 5.15))
        middle.curve(to: p(4.35, 7.25), controlPoint1: p(4.85, 5.45), controlPoint2: p(4.35, 6.2))
        middle.line(to: p(4.35, 10.75))
        middle.curve(to: p(5.85, 12.85), controlPoint1: p(4.35, 11.8), controlPoint2: p(4.85, 12.55))

        for arc in [back, middle] {
            arc.lineWidth = 1.45 * k
            arc.lineCapStyle = .round
            arc.stroke()
        }
        let front = NSBezierPath(roundedRect: NSRect(x: 7.35 * k, y: (18 - 4.5 - 9) * k, width: 9 * k, height: 9 * k), xRadius: 2.45 * k, yRadius: 2.45 * k)
        front.lineWidth = 1.5 * k
        front.stroke()

        if attention {
            // A dot on the front pane's top-right corner, with a clear gap around it.
            NSGraphicsContext.current?.saveGraphicsState()
            NSGraphicsContext.current?.compositingOperation = .clear
            NSBezierPath(ovalIn: NSRect(x: 11.6 * k, y: 11.6 * k, width: 7.2 * k, height: 7.2 * k)).fill()
            NSGraphicsContext.current?.restoreGraphicsState()
            NSBezierPath(ovalIn: NSRect(x: 12.9 * k, y: 12.9 * k, width: 4.6 * k, height: 4.6 * k)).fill()
        }
    }

    /// A template NSImage of the menu bar glyph (18 pt, as macOS expects).
    static func menuBarImage(attention: Bool = false) -> NSImage {
        let img = NSImage(size: NSSize(width: 18, height: 18), flipped: false) { rect in
            drawMenuGlyph(size: rect.width, attention: attention)
            return true
        }
        img.isTemplate = true
        return img
    }
}
