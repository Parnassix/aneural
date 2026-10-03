// Renders Aneural's app icon to a PNG.
//
// The mark is the thing the app draws: one node with hyphae growing out of it,
// lit the way the canvas lights after dark. The palette is `theme.rs`'s night
// end — the same greens the graph glows in — because an icon wants to glow and
// the daylit palette is deliberately flat.
//
//   swiftc -O scripts/icon.swift -o /tmp/aneural-icon && /tmp/aneural-icon out.png [size]

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
let size = CommandLine.arguments.count > 2 ? Int(CommandLine.arguments[2])! : 1024

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: a)
}
// theme.rs, the night end.
let background = rgb(0x02070a)
let panel = rgb(0x050e10)
let accent = rgb(0x57efb4)
let strand = rgb(0xa4ecd4)
let deep = rgb(0x2c6b61)

let space = CGColorSpaceCreateDeviceRGB()
guard let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8,
                          bytesPerRow: 0, space: space,
                          bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { exit(1) }
let s = CGFloat(size)
ctx.setAllowsAntialiasing(true)
ctx.interpolationQuality = .high

// The squircle every macOS icon lives in, inset the way the HIG grid wants.
let inset = s * 0.10
let plate = CGRect(x: inset, y: inset, width: s - inset * 2, height: s - inset * 2)
let corner = plate.width * 0.225
let squircle = CGPath(roundedRect: plate, cornerWidth: corner, cornerHeight: corner, transform: nil)

ctx.saveGState()
ctx.addPath(squircle)
ctx.clip()
ctx.setFillColor(background)
ctx.fill(plate)
// A little light from the top, so the plate is not a flat hole.
if let g = CGGradient(colorsSpace: space, colors: [panel, background] as CFArray,
                      locations: [0, 1]) {
    ctx.drawLinearGradient(g, start: CGPoint(x: 0, y: plate.maxY),
                           end: CGPoint(x: 0, y: plate.minY), options: [])
}

let mid = CGPoint(x: s / 2, y: s / 2)
let r = plate.width

/// One hypha: a cubic curve from the centre out, tapering, with a node on the end.
func hypha(angle: CGFloat, length: CGFloat, bend: CGFloat, tip: CGFloat) {
    let end = CGPoint(x: mid.x + cos(angle) * length, y: mid.y + sin(angle) * length)
    let n = CGPoint(x: -sin(angle), y: cos(angle))
    let c1 = CGPoint(x: mid.x + cos(angle) * length * 0.35 + n.x * bend * 0.6,
                     y: mid.y + sin(angle) * length * 0.35 + n.y * bend * 0.6)
    let c2 = CGPoint(x: mid.x + cos(angle) * length * 0.72 + n.x * bend,
                     y: mid.y + sin(angle) * length * 0.72 + n.y * bend)
    let path = CGMutablePath()
    path.move(to: mid)
    path.addCurve(to: end, control1: c1, control2: c2)

    ctx.setShadow(offset: .zero, blur: r * 0.035, color: accent.copy(alpha: 0.55))
    ctx.setStrokeColor(strand.copy(alpha: 0.9)!)
    ctx.setLineWidth(r * 0.019)
    ctx.setLineCap(.round)
    ctx.addPath(path)
    ctx.strokePath()

    // The node it ends in, with its own halo.
    ctx.setShadow(offset: .zero, blur: r * 0.05, color: accent.copy(alpha: 0.8))
    ctx.setFillColor(tip > 0.6 ? accent : deep)
    let rad = r * 0.021 * tip
    ctx.fillEllipse(in: CGRect(x: end.x - rad, y: end.y - rad, width: rad * 2, height: rad * 2))
}

// Six strands, deliberately uneven: the graph is grown, not drawn.
let arms: [(CGFloat, CGFloat, CGFloat, CGFloat)] = [
    (1.92, 0.315, 0.055, 1.00),
    (0.62, 0.300, -0.050, 0.85),
    (5.44, 0.275, 0.045, 1.00),
    (4.30, 0.310, -0.060, 0.70),
    (3.26, 0.290, 0.050, 0.90),
    (2.55, 0.220, -0.035, 0.60),
]
for (a, len, bend, tip) in arms {
    hypha(angle: a, length: r * len, bend: r * bend, tip: tip)
}

// The node at the middle: a bright disc inside a wide, soft halo.
ctx.setShadow(offset: .zero, blur: r * 0.13, color: accent.copy(alpha: 0.95))
ctx.setFillColor(accent)
let core = r * 0.062
ctx.fillEllipse(in: CGRect(x: mid.x - core, y: mid.y - core, width: core * 2, height: core * 2))
ctx.setShadow(offset: .zero, blur: 0, color: nil)
ctx.setFillColor(rgb(0x02100e))
let hole = core * 0.34
ctx.fillEllipse(in: CGRect(x: mid.x - hole, y: mid.y - hole, width: hole * 2, height: hole * 2))
ctx.restoreGState()

// A hairline so the plate has an edge against a dark wallpaper.
ctx.addPath(squircle)
ctx.setStrokeColor(deep.copy(alpha: 0.5)!)
ctx.setLineWidth(s * 0.0035)
ctx.strokePath()

guard let image = ctx.makeImage(),
      let dest = CGImageDestinationCreateWithURL(
          URL(fileURLWithPath: out) as CFURL, UTType.png.identifier as CFString, 1, nil)
else { exit(1) }
CGImageDestinationAddImage(dest, image, nil)
CGImageDestinationFinalize(dest)
print("wrote \(out) at \(size)x\(size)")
