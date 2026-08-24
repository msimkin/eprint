// Generates assets/eprint.icns for the Spotlight launcher bundle.
//
// Run once, by hand, and commit the result:
//     swift assets/icon.swift assets/eprint.icns
//
// Not part of `cargo build`: the icon is a fixed asset, embedded in the binary with
// `include_bytes!`, so nothing at build or run time needs Swift or AppKit.
import AppKit

// theme.rs's "brass and verdigris". The lit node is the same gold as the watch badge.
let gold    = CGColor(red: 0.82, green: 0.61, blue: 0.09, alpha: 1)
let verd    = CGColor(red: 0.33, green: 0.60, blue: 0.60, alpha: 1)
let verdDim = CGColor(red: 0.33, green: 0.60, blue: 0.60, alpha: 0.45)
let tileTop = CGColor(red: 0.09, green: 0.27, blue: 0.26, alpha: 1)
let tileBot = CGColor(red: 0.03, green: 0.13, blue: 0.13, alpha: 1)
let rgb     = CGColorSpaceCreateDeviceRGB()

func draw(_ ctx: CGContext, _ s: CGFloat) {
    // The tile. Inset and heavily rounded, which is what makes it read as an
    // application icon rather than as a picture of a lattice.
    let inset = s * 0.055
    let r = CGRect(x: inset, y: inset, width: s - 2*inset, height: s - 2*inset)
    ctx.saveGState()
    ctx.addPath(CGPath(roundedRect: r, cornerWidth: s*0.224, cornerHeight: s*0.224,
                       transform: nil))
    ctx.clip()
    ctx.drawLinearGradient(CGGradient(colorsSpace: rgb, colors: [tileTop, tileBot] as CFArray,
                                      locations: [0, 1])!,
                           start: CGPoint(x: 0, y: s), end: .zero, options: [])
    // A surface, not a swatch.
    ctx.drawLinearGradient(CGGradient(colorsSpace: rgb,
        colors: [CGColor(red: 1, green: 1, blue: 1, alpha: 0.10),
                 CGColor(red: 1, green: 1, blue: 1, alpha: 0)] as CFArray,
        locations: [0, 1])!,
        start: CGPoint(x: 0, y: s), end: CGPoint(x: 0, y: s*0.55), options: [])
    ctx.restoreGState()

    let n = 4, m = s*0.26
    let step = (s - 2*m)/CGFloat(n-1)
    // Strokes and dots are proportional, but a proportional hairline disappears
    // entirely at 16px, so both have a floor in whole pixels.
    ctx.setStrokeColor(verdDim)
    ctx.setLineWidth(max(s*0.012, 0.8))
    for i in 0..<n {
        let p = m + CGFloat(i)*step
        ctx.move(to: CGPoint(x: m, y: p)); ctx.addLine(to: CGPoint(x: s-m, y: p))
        ctx.move(to: CGPoint(x: p, y: m)); ctx.addLine(to: CGPoint(x: p, y: s-m))
    }
    ctx.strokePath()

    func dot(_ x: CGFloat, _ y: CGFloat, _ rad: CGFloat, _ c: CGColor) {
        ctx.setFillColor(c)
        ctx.fillEllipse(in: CGRect(x: x-rad, y: y-rad, width: 2*rad, height: 2*rad))
    }
    for i in 0..<n { for j in 0..<n {
        dot(m + CGFloat(i)*step, m + CGFloat(j)*step, max(s*0.030, 1.0), verd)
    }}

    // The lit node. A radial gradient, not a translucent disc: a flat disc of gold
    // over the tile reads as a muddy olive circle rather than as light.
    let lx = m + 2*step, ly = m + 2*step
    let halo = max(s*0.115, 3.0)
    ctx.saveGState()
    ctx.drawRadialGradient(CGGradient(colorsSpace: rgb,
        colors: [CGColor(red: 0.95, green: 0.72, blue: 0.15, alpha: 0.55),
                 CGColor(red: 0.95, green: 0.72, blue: 0.15, alpha: 0.0)] as CFArray,
        locations: [0, 1])!,
        startCenter: CGPoint(x: lx, y: ly), startRadius: 0,
        endCenter: CGPoint(x: lx, y: ly), endRadius: halo, options: [])
    ctx.restoreGState()
    dot(lx, ly, max(s*0.055, 1.6), gold)
}

func png(_ size: Int) -> Data {
    let c = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8,
                      bytesPerRow: 0, space: rgb,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    c.setAllowsAntialiasing(true); c.setShouldAntialias(true)
    c.interpolationQuality = .high
    draw(c, CGFloat(size))
    return NSBitmapImageRep(cgImage: c.makeImage()!).representation(using: .png,
                                                                   properties: [:])!
}

let outIcns = CommandLine.arguments[1]
let set = URL(fileURLWithPath: NSTemporaryDirectory())
    .appendingPathComponent("eprint-\(getpid()).iconset")
try! FileManager.default.createDirectory(at: set, withIntermediateDirectories: true)
// Exactly the names iconutil expects, and deliberately stopping at 256@2x — 512 real
// pixels. Adding the 512 and 512@2x layers takes the file from 191KB to 554KB, and it
// is embedded in the binary; the only thing that would ever ask for 1024 is a Retina
// Finder window at maximum icon size, looking at a launcher nobody browses to.
for (base, scale) in [(16,1),(16,2),(32,1),(32,2),(128,1),(128,2),(256,1),(256,2)] {
    let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
    try! png(base*scale).write(to: set.appendingPathComponent(name))
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set.path, "-o", outIcns]
try! p.run(); p.waitUntilExit()
try? FileManager.default.removeItem(at: set)
// A sheet to eyeball the result at the sizes that actually matter.
let sheetSizes = [512, 128, 64, 32, 16]
let W = sheetSizes.reduce(0) { $0 + $1 + 24 }
let sh = CGContext(data: nil, width: W, height: 560, bitsPerComponent: 8, bytesPerRow: 0,
                   space: rgb, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
sh.setFillColor(CGColor(red: 0.13, green: 0.13, blue: 0.14, alpha: 1))
sh.fill(CGRect(x: 0, y: 0, width: W, height: 560))
var x = 12
for sz in sheetSizes {
    let c = CGContext(data: nil, width: sz, height: sz, bitsPerComponent: 8, bytesPerRow: 0,
                      space: rgb, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    c.setAllowsAntialiasing(true); draw(c, CGFloat(sz))
    sh.draw(c.makeImage()!, in: CGRect(x: x, y: 24, width: sz, height: sz))
    x += sz + 24
}
try! NSBitmapImageRep(cgImage: sh.makeImage()!).representation(using: .png, properties: [:])!
    .write(to: URL(fileURLWithPath: (outIcns as NSString).deletingLastPathComponent + "/preview.png"))
print("wrote \(outIcns)")
