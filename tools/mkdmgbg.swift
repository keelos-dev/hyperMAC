// Draws the dmg window's background (run from the repository root: swift tools/mkdmgbg.swift):
// assets/dmg-background.tiff, 660x520 points with a 2x representation. An arrow from where
// hyperMAC.app sits to the Applications link (tools/dmg-settings.py places the icons), a line
// asking to drag, and a quiet label over the bottom row (README and the third-party notices).
import AppKit

let W: CGFloat = 660, H: CGFloat = 520

func draw(scale: CGFloat) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(W * scale), pixelsHigh: Int(H * scale),
                               bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: W, height: H)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    // Finder's y grows downward for icon positions; drawing here grows upward: flip.
    func y(_ top: CGFloat) -> CGFloat { H - top }
    NSGradient(starting: NSColor(srgbRed: 0.97, green: 0.98, blue: 1.0, alpha: 1),
               ending: NSColor(srgbRed: 0.90, green: 0.93, blue: 0.98, alpha: 1))!
        .draw(in: NSRect(x: 0, y: 0, width: W, height: H), angle: -90)
    // the bottom band behind the notices
    NSColor(srgbRed: 0.85, green: 0.88, blue: 0.94, alpha: 1).setFill()
    NSRect(x: 0, y: 0, width: W, height: 190).fill()

    let center = NSMutableParagraphStyle()
    center.alignment = .center
    func text(_ s: String, top: CGFloat, size: CGFloat, weight: NSFont.Weight, gray: CGFloat) {
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: size, weight: weight),
            .foregroundColor: NSColor(white: gray, alpha: 1),
            .paragraphStyle: center,
        ]
        NSAttributedString(string: s, attributes: attrs).draw(in: NSRect(x: 0, y: y(top) - size * 1.4, width: W, height: size * 1.6))
    }
    text("Drag hyperMAC to Applications", top: 34, size: 18, weight: .semibold, gray: 0.20)
    text("把 hyperMAC 拖到「应用程序」", top: 62, size: 15, weight: .regular, gray: 0.35)

    // arrow between the two icons (centres at x 170 and 490, y 190 from the top)
    let ay = y(190)
    let p = NSBezierPath()
    p.lineWidth = 7
    p.lineCapStyle = .round
    p.lineJoinStyle = .round
    p.move(to: NSPoint(x: 262, y: ay))
    p.line(to: NSPoint(x: 398, y: ay))
    p.move(to: NSPoint(x: 376, y: ay + 20))
    p.line(to: NSPoint(x: 400, y: ay))
    p.line(to: NSPoint(x: 376, y: ay - 20))
    NSColor(srgbRed: 0.16, green: 0.45, blue: 0.90, alpha: 1).setStroke()
    p.stroke()

    text("Read me · Third-party notices / 说明 · 第三方声明", top: 346, size: 11, weight: .regular, gray: 0.45)
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

let tmp = NSTemporaryDirectory()
let one = tmp + "dmgbg.png", two = tmp + "dmgbg@2x.png"
try! draw(scale: 1).representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: one))
try! draw(scale: 2).representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: two))
let out = "assets/dmg-background.tiff"
let t = Process()
t.executableURL = URL(fileURLWithPath: "/usr/bin/tiffutil")
t.arguments = ["-cathidpicheck", one, two, "-out", out]
try! t.run()
t.waitUntilExit()
print(t.terminationStatus == 0 ? "wrote \(out)" : "tiffutil failed")
exit(t.terminationStatus)
