// Draws hyperMAC's icon and writes assets/hypermac.icns (run from the repository root:
// swift tools/mkicon.swift). The design the user picked (2026-11-10): three rounded "window"
// cards stacked up and to the right (cyan at the back, light blue, then a blue one in front with
// three dots and "HM"), tilted a little clockwise, white gaps between them. Transparent
// background, redrawn as shapes so every size is sharp. Only AppKit drawing.
import AppKit

func color(_ hex: UInt32) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: 1)
}

func draw(_ px: Int) -> Data {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = CGFloat(px) / 1024
    // everything in 1024 units, tilted about the centre
    let t = NSAffineTransform()
    t.scale(by: s)
    t.translateX(by: 512, yBy: 512)
    t.rotate(byDegrees: -4)
    t.translateX(by: -512, yBy: -512)
    t.concat()

    let (w, h, radius): (CGFloat, CGFloat, CGFloat) = (600, 470, 84)
    let step = NSPoint(x: 108, y: 80) // each card behind: up and to the right
    let origin = NSPoint(x: 512 - (w + 2 * step.x) / 2, y: 512 - (h + 2 * step.y) / 2 - 10)
    func card(_ i: CGFloat) -> NSRect {
        NSRect(x: origin.x + i * step.x, y: origin.y + i * step.y, width: w, height: h)
    }
    func rounded(_ r: NSRect, grow: CGFloat = 0) -> NSBezierPath {
        let g = r.insetBy(dx: -grow, dy: -grow)
        return NSBezierPath(roundedRect: g, xRadius: radius + grow, yRadius: radius + grow)
    }
    let gap: CGFloat = 20 // the white edge around the cards in front

    // back: cyan
    color(0x2ed9dc).setFill()
    rounded(card(2)).fill()
    // middle: light blue, white edge
    NSColor.white.setFill()
    rounded(card(1), grow: gap).fill()
    NSGradient(starting: color(0x2ab0f2), ending: color(0x1a95ea))!.draw(in: rounded(card(1)), angle: -90)
    // front: blue, white edge
    NSColor.white.setFill()
    rounded(card(0), grow: gap).fill()
    let front = card(0)
    NSGradient(starting: color(0x2a7bea), ending: color(0x0654d8))!.draw(in: rounded(front), angle: -90)

    // three dots, top left
    NSColor.white.setFill()
    for i in 0..<3 {
        let d: CGFloat = 46
        NSBezierPath(ovalIn: NSRect(x: front.minX + 62 + CGFloat(i) * 66, y: front.maxY - 62 - d, width: d, height: d)).fill()
    }
    // "HM": heavy white type, a little below the middle
    let font = NSFont.systemFont(ofSize: 300, weight: .black)
    let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.white, .kern: -6]
    let text = NSAttributedString(string: "HM", attributes: attrs)
    let size = text.size()
    text.draw(at: NSPoint(x: front.midX - size.width / 2,
                          y: front.midY - 30 - font.capHeight / 2 + font.descender))

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

let fm = FileManager.default
let set = "assets/hypermac.iconset"
try? fm.removeItem(atPath: set)
try! fm.createDirectory(atPath: set, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    try! draw(base).write(to: URL(fileURLWithPath: "\(set)/icon_\(base)x\(base).png"))
    try! draw(base * 2).write(to: URL(fileURLWithPath: "\(set)/icon_\(base)x\(base)@2x.png"))
}
try! draw(1024).write(to: URL(fileURLWithPath: "assets/hypermac-1024.png"))
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", "assets/hypermac.icns"]
try! p.run()
p.waitUntilExit()
try? fm.removeItem(atPath: set)
print("assets/hypermac.icns")

// The wordmark (user 2026-11-10): "hyper" in navy, "MAC" in a blue -> cyan gradient, heavy
// italic, transparent. assets/wordmark.png (height 64) and wordmark@2x.png (128).
func wordmark(height: Int, hyperColor: NSColor = color(0x14246b)) -> Data {
    let sz = CGFloat(height)
    // heavy italic: withSymbolicTraits(.italic) alone drops the weight; ask for both
    let base = NSFont.systemFont(ofSize: sz * 0.92, weight: .bold)
    let desc = base.fontDescriptor.addingAttributes([
        .traits: [NSFontDescriptor.TraitKey.weight: NSFont.Weight.bold, NSFontDescriptor.TraitKey.slant: 0.2],
    ])
    let font = NSFont(descriptor: desc, size: sz * 0.92) ?? base
    let hyper = NSAttributedString(string: "hyper", attributes: [.font: font, .foregroundColor: hyperColor, .kern: -sz * 0.01])
    let mac = NSAttributedString(string: "MAC", attributes: [.font: font, .foregroundColor: NSColor.black, .kern: sz * 0.01])
    let (hw, mw) = (hyper.size().width, mac.size().width)
    let width = Int((hw + mw + sz * 0.12).rounded(.up))
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    let ctx = NSGraphicsContext(bitmapImageRep: rep)!
    NSGraphicsContext.current = ctx
    // baseline so that capitals and descenders fit: the line's bottom at |descender|
    let y = -font.descender * 0.55
    hyper.draw(at: NSPoint(x: sz * 0.02, y: y))
    // "MAC": its shape as a mask, then the gradient through it
    let mx = sz * 0.02 + hw + sz * 0.04
    let mask = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height, bitsPerSample: 8,
                                samplesPerPixel: 1, hasAlpha: false, isPlanar: false,
                                colorSpaceName: .deviceWhite, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: mask)
    NSColor.black.setFill()
    NSRect(x: 0, y: 0, width: width, height: height).fill()
    NSAttributedString(string: "MAC", attributes: [.font: font, .foregroundColor: NSColor.white, .kern: sz * 0.01])
        .draw(at: NSPoint(x: mx, y: y))
    NSGraphicsContext.restoreGraphicsState()
    let cg = ctx.cgContext
    cg.saveGState()
    cg.clip(to: CGRect(x: 0, y: 0, width: width, height: height), mask: mask.cgImage!)
    let g = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
                       colors: [color(0x1652e0).cgColor, color(0x1a8cec).cgColor, color(0x22d0e4).cgColor] as CFArray,
                       locations: [0, 0.55, 1])!
    cg.drawLinearGradient(g, start: CGPoint(x: mx, y: 0), end: CGPoint(x: mx + mw, y: sz), options: [])
    cg.restoreGState()
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}
try! wordmark(height: 64).write(to: URL(fileURLWithPath: "assets/wordmark.png"))
try! wordmark(height: 128).write(to: URL(fileURLWithPath: "assets/wordmark@2x.png"))
// dark appearance: "hyper" in white (navy disappears on a dark window)
try! wordmark(height: 128, hyperColor: .white).write(to: URL(fileURLWithPath: "assets/wordmark-dark@2x.png"))
print("assets/wordmark.png")
