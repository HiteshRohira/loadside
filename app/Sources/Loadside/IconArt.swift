import AppKit

/// Vector artwork for the app icon and the menu bar glyph. Shared by the app
/// (menu bar icon, drawn at runtime as a template image) and
/// scripts/make-icon.swift (the .icns). All drawing is in a unit square with
/// AppKit's y-up coordinates, scaled to the target size.
enum IconArt {
    enum Style: String, CaseIterable {
        case loop, sideDoor, signed, monogram

        var title: String {
            switch self {
            case .loop: "A · Refresh loop"
            case .sideDoor: "B · Side door"
            case .signed: "C · Signed"
            case .monogram: "D · L monogram"
            }
        }

        var blurb: String {
            switch self {
            case .loop: "A phone inside a refresh arrow: it keeps your apps signed."
            case .sideDoor: "An arrow entering the side of a phone: literally side-loading."
            case .signed: "A phone with a signature seal: signed with your Apple ID."
            case .monogram: "A bold L whose foot is an arrow loading in from the side."
            }
        }
    }

    /// The style the app ships with.
    static let current: Style = .loop

    // MARK: Helpers

    private static func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> NSColor {
        NSColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: a)
    }

    private static func rr(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat, _ r: CGFloat, _ s: CGFloat) -> NSBezierPath {
        NSBezierPath(roundedRect: NSRect(x: x * s, y: y * s, width: w * s, height: h * s), xRadius: r * s, yRadius: r * s)
    }

    /// A filled triangular arrowhead with its tip at `tip`, pointing along `angle` (radians).
    private static func arrowhead(tip: CGPoint, angle: CGFloat, length: CGFloat, width: CGFloat) -> NSBezierPath {
        let back = CGPoint(x: tip.x - cos(angle) * length, y: tip.y - sin(angle) * length)
        let nx = -sin(angle) * width / 2, ny = cos(angle) * width / 2
        let p = NSBezierPath()
        p.move(to: tip)
        p.line(to: CGPoint(x: back.x + nx, y: back.y + ny))
        p.line(to: CGPoint(x: back.x - nx, y: back.y - ny))
        p.close()
        return p
    }

    // MARK: App icon

    /// Draws the full-colour app icon into the current context at `s` points.
    static func drawAppIcon(_ style: Style, size s: CGFloat) {
        // macOS icon grid: 824/1024 body, ~22.5% corner radius, soft shadow.
        let inset: CGFloat = 100 / 1024
        let body = rr(inset, inset, 1 - inset * 2, 1 - inset * 2, (1 - inset * 2) * 0.225, s)
        let (c1, c2): (NSColor, NSColor) = {
            switch style {
            case .loop: (rgb(0x1E1446), rgb(0x3B1E8C))
            case .sideDoor: (rgb(0x4338CA), rgb(0x8B5CF6))
            case .signed: (rgb(0x15151B), rgb(0x2B2B36))
            case .monogram: (rgb(0xFF5F3A), rgb(0xE0287D))
            }
        }()
        NSGraphicsContext.current?.saveGraphicsState()
        let shadow = NSShadow()
        shadow.shadowBlurRadius = s * 0.028
        shadow.shadowOffset = NSSize(width: 0, height: -s * 0.012)
        shadow.shadowColor = NSColor.black.withAlphaComponent(0.35)
        shadow.set()
        NSGradient(colors: [c1, c2])!.draw(in: body, angle: 90 + 25)
        NSGraphicsContext.current?.restoreGraphicsState()

        // Subtle top sheen.
        NSGraphicsContext.current?.saveGraphicsState()
        body.addClip()
        NSGradient(colors: [NSColor.white.withAlphaComponent(0.10), NSColor.white.withAlphaComponent(0)])!
            .draw(in: NSRect(x: 0, y: s * 0.5, width: s, height: s * 0.5), angle: -90)
        NSGraphicsContext.current?.restoreGraphicsState()

        switch style {
        case .loop: drawLoopArt(s)
        case .sideDoor: drawSideDoorArt(s)
        case .signed: drawSignedArt(s)
        case .monogram: drawMonogramArt(s)
        }
    }

    private static func phoneOutline(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat, _ s: CGFloat, line: CGFloat, color: NSColor) {
        let p = rr(x, y, w, h, w * 0.24, s)
        p.lineWidth = line * s
        color.setStroke()
        p.stroke()
        // Dynamic-island pill.
        color.setFill()
        rr(x + w * 0.36, y + h - h * 0.12, w * 0.28, h * 0.045, h * 0.0225, s).fill()
    }

    /// A circular arrow: the arc (round tail) and the head as separate paths.
    /// Filled one by one, so overlaps never cancel out. Angles in degrees,
    /// counter-clockwise.
    static func loopParts(center c: CGPoint, radius r: CGFloat, width w: CGFloat, from a0: CGFloat, to a1: CGFloat, head: CGFloat) -> [CGPath] {
        let arc = CGMutablePath()
        arc.addArc(center: c, radius: r, startAngle: a0 * .pi / 180, endAngle: a1 * .pi / 180, clockwise: false)
        let stroked = arc.copy(strokingWithWidth: w, lineCap: .round, lineJoin: .round, miterLimit: 10)
        let t = a1 * .pi / 180
        let e = CGPoint(x: c.x + cos(t) * r, y: c.y + sin(t) * r)
        let d = CGPoint(x: -sin(t), y: cos(t))          // direction of travel
        let n = CGPoint(x: cos(t), y: sin(t))           // outward normal
        let tri = CGMutablePath()
        tri.move(to: CGPoint(x: e.x + d.x * head * 0.9, y: e.y + d.y * head * 0.9))
        tri.addLine(to: CGPoint(x: e.x + n.x * head * 0.62, y: e.y + n.y * head * 0.62))
        tri.addLine(to: CGPoint(x: e.x - n.x * head * 0.62, y: e.y - n.y * head * 0.62))
        tri.closeSubpath()
        return [stroked, tri]
    }

    private static func drawLoopArt(_ s: CGFloat) {
        let ctx = NSGraphicsContext.current!.cgContext
        for part in loopParts(center: CGPoint(x: 0.5 * s, y: 0.5 * s), radius: 0.27 * s, width: 0.06 * s, from: 125, to: 385, head: 0.11 * s) {
            ctx.saveGState()
            ctx.addPath(part)
            ctx.clip()
            NSGradient(colors: [rgb(0x5EEAD4), rgb(0x60A5FA), rgb(0xC084FC)])!.draw(in: NSRect(x: 0, y: 0, width: s, height: s), angle: -60)
            ctx.restoreGState()
        }
        phoneOutline(0.41, 0.355, 0.18, 0.29, s, line: 0.03, color: .white)
    }

    private static func drawSideDoorArt(_ s: CGFloat) {
        // Phone on the right; an arrow slides in through its left side.
        phoneOutline(0.47, 0.25, 0.30, 0.50, s, line: 0.042, color: .white)
        let y = 0.5 * s
        let shaft = NSBezierPath()
        shaft.move(to: CGPoint(x: 0.19 * s, y: y))
        shaft.line(to: CGPoint(x: 0.55 * s, y: y))
        shaft.lineWidth = 0.07 * s
        shaft.lineCapStyle = .round
        NSColor.white.setStroke()
        shaft.stroke()
        NSColor.white.setFill()
        arrowhead(tip: CGPoint(x: 0.68 * s, y: y), angle: 0, length: 0.15 * s, width: 0.19 * s).fill()
        // Gap in the phone's edge where the arrow passes (feels like it enters).
        let c1 = rgb(0x4338CA)
        _ = c1
    }

    private static func drawSignedArt(_ s: CGFloat) {
        // Glowing violet screen phone.
        let phone = rr(0.30, 0.20, 0.36, 0.60, 0.085, s)
        NSGradient(colors: [rgb(0x7C3AED), rgb(0x4F46E5)])!.draw(in: phone, angle: -90)
        let rim = rr(0.30, 0.20, 0.36, 0.60, 0.085, s)
        rim.lineWidth = 0.02 * s
        NSColor.white.withAlphaComponent(0.85).setStroke()
        rim.stroke()
        NSColor.white.withAlphaComponent(0.9).setFill()
        rr(0.43, 0.735, 0.10, 0.022, 0.011, s).fill()
        // A signature stroke on the screen.
        let sig = NSBezierPath()
        sig.move(to: CGPoint(x: 0.36 * s, y: 0.46 * s))
        sig.curve(to: CGPoint(x: 0.45 * s, y: 0.50 * s), controlPoint1: CGPoint(x: 0.39 * s, y: 0.58 * s), controlPoint2: CGPoint(x: 0.43 * s, y: 0.58 * s))
        sig.curve(to: CGPoint(x: 0.50 * s, y: 0.47 * s), controlPoint1: CGPoint(x: 0.46 * s, y: 0.44 * s), controlPoint2: CGPoint(x: 0.48 * s, y: 0.43 * s))
        sig.curve(to: CGPoint(x: 0.60 * s, y: 0.53 * s), controlPoint1: CGPoint(x: 0.53 * s, y: 0.52 * s), controlPoint2: CGPoint(x: 0.56 * s, y: 0.55 * s))
        sig.lineWidth = 0.022 * s
        sig.lineCapStyle = .round
        NSColor.white.setStroke()
        sig.stroke()
        // Seal badge, bottom right.
        let badge = NSBezierPath(ovalIn: NSRect(x: 0.55 * s, y: 0.15 * s, width: 0.26 * s, height: 0.26 * s))
        NSGradient(colors: [rgb(0x34D399), rgb(0x10B981)])!.draw(in: badge, angle: -90)
        let ring = NSBezierPath(ovalIn: NSRect(x: 0.55 * s, y: 0.15 * s, width: 0.26 * s, height: 0.26 * s))
        ring.lineWidth = 0.018 * s
        rgb(0x15151B).setStroke()
        ring.stroke()
        let check = NSBezierPath()
        check.move(to: CGPoint(x: 0.615 * s, y: 0.285 * s))
        check.line(to: CGPoint(x: 0.665 * s, y: 0.235 * s))
        check.line(to: CGPoint(x: 0.75 * s, y: 0.33 * s))
        check.lineWidth = 0.035 * s
        check.lineCapStyle = .round
        check.lineJoinStyle = .round
        NSColor.white.setStroke()
        check.stroke()
    }

    private static func drawMonogramArt(_ s: CGFloat) {
        NSColor.white.setFill()
        // Vertical stem of the L (reads as a phone edge).
        rr(0.30, 0.24, 0.13, 0.54, 0.065, s).fill()
        // Foot: an arrow pointing back into the stem (loading in from the side).
        let shaft = NSBezierPath()
        shaft.move(to: CGPoint(x: 0.74 * s, y: 0.305 * s))
        shaft.line(to: CGPoint(x: 0.60 * s, y: 0.305 * s))
        shaft.lineWidth = 0.13 * s
        shaft.lineCapStyle = .round
        NSColor.white.setStroke()
        shaft.stroke()
        arrowhead(tip: CGPoint(x: 0.445 * s, y: 0.305 * s), angle: .pi, length: 0.13 * s, width: 0.24 * s).fill()
    }

    // MARK: Menu bar glyph (template: drawn in black, macOS tints it)

    static func drawMenuGlyph(_ style: Style, size s: CGFloat, attention: Bool = false) {
        NSColor.black.setStroke()
        NSColor.black.setFill()
        let lw = s * 0.09
        func phone(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) {
            let p = rr(x, y, w, h, w * 0.26, s)
            p.lineWidth = lw
            p.stroke()
        }
        switch style {
        case .loop:
            let ctx = NSGraphicsContext.current!.cgContext
            for part in loopParts(center: CGPoint(x: 0.5 * s, y: 0.5 * s), radius: 0.39 * s, width: lw, from: 125, to: 385, head: 0.17 * s) {
                ctx.addPath(part)
                ctx.fillPath()
            }
            phone(0.39, 0.30, 0.22, 0.40)
        case .sideDoor:
            phone(0.46, 0.12, 0.40, 0.76)
            let y = 0.5 * s
            let shaft = NSBezierPath()
            shaft.move(to: CGPoint(x: 0.06 * s, y: y))
            shaft.line(to: CGPoint(x: 0.52 * s, y: y))
            shaft.lineWidth = lw * 1.1
            shaft.lineCapStyle = .round
            shaft.stroke()
            arrowhead(tip: CGPoint(x: 0.72 * s, y: y), angle: 0, length: 0.24 * s, width: 0.32 * s).fill()
        case .signed:
            phone(0.16, 0.08, 0.48, 0.84)
            let badge = NSBezierPath(ovalIn: NSRect(x: 0.50 * s, y: 0.04 * s, width: 0.46 * s, height: 0.46 * s))
            NSGraphicsContext.current?.saveGraphicsState()
            NSGraphicsContext.current?.compositingOperation = .clear
            NSBezierPath(ovalIn: NSRect(x: 0.44 * s, y: -0.02 * s, width: 0.58 * s, height: 0.58 * s)).fill()
            NSGraphicsContext.current?.restoreGraphicsState()
            badge.fill()
            NSGraphicsContext.current?.saveGraphicsState()
            NSGraphicsContext.current?.compositingOperation = .clear
            let check = NSBezierPath()
            check.move(to: CGPoint(x: 0.61 * s, y: 0.27 * s))
            check.line(to: CGPoint(x: 0.70 * s, y: 0.18 * s))
            check.line(to: CGPoint(x: 0.85 * s, y: 0.35 * s))
            check.lineWidth = lw * 0.9
            check.lineCapStyle = .round
            check.lineJoinStyle = .round
            check.stroke()
            NSGraphicsContext.current?.restoreGraphicsState()
        case .monogram:
            rr(0.20, 0.10, 0.20, 0.80, 0.10, s).fill()
            let shaft = NSBezierPath()
            shaft.move(to: CGPoint(x: 0.90 * s, y: 0.20 * s))
            shaft.line(to: CGPoint(x: 0.66 * s, y: 0.20 * s))
            shaft.lineWidth = 0.20 * s
            shaft.lineCapStyle = .round
            shaft.stroke()
            arrowhead(tip: CGPoint(x: 0.42 * s, y: 0.20 * s), angle: .pi, length: 0.22 * s, width: 0.38 * s).fill()
        }
        if attention {
            // Small dot, top right, punched out of the glyph.
            NSGraphicsContext.current?.saveGraphicsState()
            NSGraphicsContext.current?.compositingOperation = .clear
            NSBezierPath(ovalIn: NSRect(x: 0.66 * s, y: 0.66 * s, width: 0.36 * s, height: 0.36 * s)).fill()
            NSGraphicsContext.current?.restoreGraphicsState()
            NSBezierPath(ovalIn: NSRect(x: 0.72 * s, y: 0.72 * s, width: 0.26 * s, height: 0.26 * s)).fill()
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
