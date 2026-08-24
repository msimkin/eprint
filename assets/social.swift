// Generates the 1280x640 image for GitHub's repository social preview — the card
// that renders wherever a link to the repo is posted.
//
// Run by hand, like assets/icon.swift; GitHub has no API for this, so the result is
// uploaded once through Settings -> General -> Social preview.
//
//     swift assets/social.swift assets/eprint.icns assets/social-preview.png
//
// The icon is *composited from the .icns*, never redrawn. An earlier draft copied the
// drawing code out of icon.swift, which would have let the card and the app disagree
// the first time the icon changed.
import AppKit

let rgb = CGColorSpaceCreateDeviceRGB()
let icnsPath = CommandLine.arguments[1]
let outPath = CommandLine.arguments[2]

guard let art = NSImage(contentsOfFile: icnsPath) else {
    FileHandle.standardError.write("cannot read \(icnsPath)\n".data(using: .utf8)!)
    exit(1)
}

// GitHub renders at 1280x640 and trims the edges, so everything sits well inside.
let W = 1280, H = 640
// Opaque, with no alpha channel at all: the card is a solid rectangle, so an alpha
// channel is 58KB of nothing, and GitHub's uploader is the sort of thing best handed
// the plainest possible file. sRGB explicitly, rather than device RGB, so the colours
// do not shift once it is served on the web.
let ctx = CGContext(data: nil, width: W, height: H, bitsPerComponent: 8, bytesPerRow: 0,
                    space: CGColorSpace(name: CGColorSpace.sRGB)!,
                    bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
ctx.setAllowsAntialiasing(true)
ctx.interpolationQuality = .high

ctx.drawLinearGradient(
    CGGradient(colorsSpace: rgb, colors: [
        CGColor(red: 0.055, green: 0.16, blue: 0.155, alpha: 1),
        CGColor(red: 0.02, green: 0.075, blue: 0.075, alpha: 1)] as CFArray,
        locations: [0, 1])!,
    start: CGPoint(x: 0, y: CGFloat(H)), end: .zero, options: [])

// Largest representation the icns holds, so the card is never drawn from a small one.
let best = art.representations.max(by: { $0.pixelsWide < $1.pixelsWide })!
let box = CGRect(x: 110, y: 190, width: 260, height: 260)
if let cg = (best as? NSBitmapImageRep)?.cgImage {
    ctx.draw(cg, in: box)
} else {
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)
    art.draw(in: box)
    NSGraphicsContext.restoreGraphicsState()
}

func text(_ s: String, _ x: CGFloat, _ y: CGFloat, _ size: CGFloat,
          _ weight: NSFont.Weight, _ color: NSColor, mono: Bool = false) {
    let f = mono ? NSFont.monospacedSystemFont(ofSize: size, weight: weight)
                 : NSFont.systemFont(ofSize: size, weight: weight)
    let a = NSAttributedString(string: s, attributes: [.font: f, .foregroundColor: color])
    ctx.textPosition = CGPoint(x: x, y: y)
    CTLineDraw(CTLineCreateWithAttributedString(a), ctx)
}

let teal = NSColor(srgbRed: 0.52, green: 0.76, blue: 0.75, alpha: 1)
let dim = NSColor(srgbRed: 0.42, green: 0.58, blue: 0.58, alpha: 1)
// Monospaced, because that is what the thing is.
text("eprint", 440, 372, 104, .semibold, .white, mono: true)
text("Search the IACR Cryptology ePrint Archive", 446, 300, 36, .regular, teal)
text("from the command line", 446, 252, 36, .regular, teal)
text("26,000 papers · offline full-text search · watches · notifications",
     446, 186, 23, .regular, dim)

let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
try! rep.representation(using: .png, properties: [:])!
    .write(to: URL(fileURLWithPath: outPath))
print("wrote \(outPath)")
