// Renders the Loadside app icon (IconArt.current) into an .iconset.
// swiftc -parse-as-library scripts/make-icon.swift app/Sources/Loadside/IconArt.swift -o make-icon && ./make-icon OUT.iconset
import AppKit

@main struct MakeIcon {
    static func render(_ px: Int) -> Data {
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        IconArt.drawAppIcon(IconArt.current, size: CGFloat(px))
        NSGraphicsContext.restoreGraphicsState()
        return rep.representation(using: .png, properties: [:])!
    }

    static func main() throws {
        let out = URL(fileURLWithPath: CommandLine.arguments[1])
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        for base in [16, 32, 128, 256, 512] {
            try render(base).write(to: out.appendingPathComponent("icon_\(base)x\(base).png"))
            try render(base * 2).write(to: out.appendingPathComponent("icon_\(base)x\(base)@2x.png"))
        }
    }
}
