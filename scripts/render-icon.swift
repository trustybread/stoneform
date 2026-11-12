// Renders an SVG to a square PNG: swift scripts/render-icon.swift <in.svg> <pixels> <out.png>
// Uses AppKit's own SVG support, so making the icon needs nothing installed.
import AppKit

let args = CommandLine.arguments
guard args.count == 4, let pixels = Int(args[2]), pixels > 0 else {
    FileHandle.standardError.write("usage: render-icon.swift <in.svg> <pixels> <out.png>\n".data(using: .utf8)!)
    exit(2)
}
guard let image = NSImage(contentsOf: URL(fileURLWithPath: args[1])) else {
    FileHandle.standardError.write("cannot read \(args[1])\n".data(using: .utf8)!)
    exit(1)
}
let bitmap = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
bitmap.size = NSSize(width: pixels, height: pixels)
NSGraphicsContext.saveGraphicsState()
let context = NSGraphicsContext(bitmapImageRep: bitmap)!
context.imageInterpolation = .high
NSGraphicsContext.current = context
image.draw(in: NSRect(x: 0, y: 0, width: pixels, height: pixels))
NSGraphicsContext.restoreGraphicsState()
try bitmap.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[3]))
