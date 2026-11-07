import SwiftUI
import AppKit
import CoreImage

struct MobileShareView: View {
    let url: String?
    @State private var qr: NSImage?
    @State private var copied = false

    var body: some View {
        VStack(spacing: 14) {
            Text("モバイル版を開く").font(.headline)
            if let url {
                if let qr {
                    Image(nsImage: qr).interpolation(.none).resizable().scaledToFit()
                        .frame(width: 240, height: 240).padding(16).background(.white)
                        .accessibilityLabel("モバイル版のURLを読み取るQRコード")
                } else {
                    Text("QRコードを生成できませんでした。URLをコピーしてください。")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Text(url).font(.system(size: 12, design: .monospaced))
                    .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button {
                    NSPasteboard.general.clearContents()
                    copied = NSPasteboard.general.setString(url, forType: .string)
                } label: {
                    Label(copied ? "コピーしました" : "URLをコピー", systemImage: copied ? "checkmark" : "doc.on.doc")
                }
            } else {
                Text("モバイル版のURLがありません。収録中で、リモート閲覧の公開URLが設定されているか確認してください。")
                    .font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(20).frame(width: 340)
        .task(id: url) {
            copied = false
            qr = url.flatMap(Self.qrImage)
        }
    }

    static func qrImage(_ url: String) -> NSImage? {
        guard let filter = CIFilter(name: "CIQRCodeGenerator") else { return nil }
        filter.setValue(Data(url.utf8), forKey: "inputMessage")
        filter.setValue("M", forKey: "inputCorrectionLevel")
        guard let output = filter.outputImage else { return nil }
        let background = CIImage(color: CIColor.white).cropped(to: output.extent.insetBy(dx: -4, dy: -4))
        let scaled = output.composited(over: background).transformed(by: CGAffineTransform(scaleX: 8, y: 8))
        guard let image = CIContext().createCGImage(scaled, from: scaled.extent) else { return nil }
        return NSImage(cgImage: image, size: NSSize(width: image.width, height: image.height))
    }
}
