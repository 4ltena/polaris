// macOS配布画面の背景と、既存SVGのシンボルを使ったアプリアイコンを描画する。
// swift render-installer.swift /absolute/output /absolute/polaris-banner-white.svg
import AppKit

guard CommandLine.arguments.count == 3 else {
    fatalError("出力先と既存のバナーSVGを指定してください。")
}
let output = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
guard let logo = NSImage(contentsOfFile: CommandLine.arguments[2]) else {
    fatalError("既存のPolarisロゴを読み込めません。")
}

func color(_ red: CGFloat, _ green: CGFloat, _ blue: CGFloat, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: red, green: green, blue: blue, alpha: alpha)
}

func png(_ name: String, width: Int, height: Int, draw: () -> Void) throws {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    draw()
    NSGraphicsContext.restoreGraphicsState()
    try bitmap.representation(using: .png, properties: [:])!.write(to: output.appendingPathComponent(name))
}

// Finderの論理サイズは640×520。背景はRetina用に2倍で描画する。
try png("background@2x.png", width: 1280, height: 1040) {
    let transform = NSAffineTransform()
    transform.scale(by: 2)
    transform.concat()
    let frame = NSRect(x: 0, y: 0, width: 640, height: 520)
    NSGradient(starting: color(0.035, 0.065, 0.10), ending: color(0.07, 0.12, 0.18))!
        .draw(in: frame, angle: -90)
    // 決定的な星配置。ロゴとラベルの周囲は空ける。
    for index in 0..<72 {
        let x = CGFloat((index * 137 + 23) % 640)
        let y = CGFloat((index * 71 + 17) % 430 + 90)
        if abs(x - 320) < 85 && y > 315 { continue }
        color(0.80, 0.88, 0.97, CGFloat(index % 5 + 2) / 15).setFill()
        let size: CGFloat = index % 9 == 0 ? 1.6 : 0.8
        NSBezierPath(ovalIn: NSRect(x: x, y: y, width: size, height: size)).fill()
    }
    let earth = NSBezierPath(ovalIn: NSRect(x: -110, y: -575, width: 860, height: 860))
    NSGraphicsContext.saveGraphicsState()
    earth.addClip()
    NSGradient(starting: color(0.025, 0.06, 0.10), ending: color(0.14, 0.28, 0.39))!
        .draw(in: frame, angle: 90)
    NSGraphicsContext.restoreGraphicsState()
    NSGraphicsContext.saveGraphicsState()
    let glow = NSShadow()
    glow.shadowColor = color(0.41, 0.68, 0.87, 0.65)
    glow.shadowBlurRadius = 12
    glow.shadowOffset = .zero
    glow.set()
    color(0.53, 0.74, 0.90, 0.85).setStroke()
    earth.lineWidth = 1.3
    earth.stroke()
    NSGraphicsContext.restoreGraphicsState()
    // アプリからApplicationsへの移動方向。アイコン自体はFinderに配置する。
    color(0.75, 0.84, 0.91, 0.7).setStroke()
    let arrow = NSBezierPath()
    arrow.move(to: NSPoint(x: 320, y: 300))
    arrow.line(to: NSPoint(x: 320, y: 254))
    arrow.move(to: NSPoint(x: 313, y: 261))
    arrow.line(to: NSPoint(x: 320, y: 254))
    arrow.line(to: NSPoint(x: 327, y: 261))
    arrow.lineWidth = 1.2
    arrow.stroke()
    // Finderは画像背景の上でも黒いファイル名を使うため、名前の下だけ明るくする。
    color(0.76, 0.84, 0.90, 0.94).setFill()
    for y: CGFloat in [325, 57] {
        NSBezierPath(roundedRect: NSRect(x: 242, y: y, width: 156, height: 29),
            xRadius: 7, yRadius: 7).fill()
    }
    let paragraph = NSMutableParagraphStyle()
    paragraph.alignment = .center
    ("PolarisをApplicationsへドラッグ" as NSString).draw(
        in: NSRect(x: 40, y: 32, width: 560, height: 22), withAttributes: [
            .font: NSFont.systemFont(ofSize: 13), .foregroundColor: color(0.74, 0.82, 0.88),
            .paragraphStyle: paragraph
        ])
}

let iconset = output.appendingPathComponent("Polaris.iconset", isDirectory: true)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let size = points * scale
        let filename = "Polaris.iconset/icon_\(points)x\(points)\(scale == 2 ? "@2x" : "").png"
        try png(filename, width: size, height: size) {
            let transform = NSAffineTransform()
            transform.scale(by: CGFloat(size) / 1024)
            transform.concat()
            let tile = NSBezierPath(roundedRect: NSRect(x: 64, y: 64, width: 896, height: 896),
                xRadius: 204, yRadius: 204)
            NSGradient(starting: color(0.04, 0.075, 0.12), ending: color(0.10, 0.19, 0.27))!
                .draw(in: tile, angle: 70)
            // 元SVGのシンボル領域だけを切り出す。字形や6本線を再作図しない。
            logo.draw(in: NSRect(x: 268, y: 242, width: 488, height: 540),
                from: NSRect(x: 279, y: 102, width: 188, height: 208),
                operation: .sourceOver, fraction: 1)
        }
    }
}
print("インストーラーの背景とアイコンを作成しました：\(output.path)")
