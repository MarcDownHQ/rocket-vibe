import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins
import RocketVibeCore
import RocketVibeKit
import SwiftUI

/// Connect WhatsApp: the QR the phone scans (Linked devices), or by number a
/// code to type on the phone. A click outside, Escape or Cancel stops the
/// attempt and erases its state; once linked, the account opens.
struct WhatsAppLinkSheet: View {
    @Environment(AppModel.self) var app
    @Bindable var model: WhatsAppLinkModel
    @State private var byNumber = false

    var body: some View {
        VStack(spacing: 16) {
            Text(L("wa.link_title")).font(.title3.bold())
            Text(L("wa.link_help"))
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            if let qr = model.state?.qr, let image = Self.image(qr) {
                Image(nsImage: image)
                    .interpolation(.none)
                    .resizable()
                    .frame(width: 260, height: 260)
                    .accessibilityLabel(L("wa.qr"))
            } else {
                ProgressView().frame(width: 260, height: 260)
            }
            Text(model.state?.status ?? L("wa.connecting"))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            if model.state?.retry == true {
                Button(L("wa.reconnect")) { model.retry() }
            }
            DisclosureGroup(L("wa.pair_number"), isExpanded: $byNumber) {
                VStack(alignment: .leading, spacing: 8) {
                    Text(L("wa.pair_number_help")).font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    TextField("+33 6 12 34 56 78", text: $model.number)
                        .textContentType(.telephoneNumber)
                        .onSubmit { model.askCode() }
                    Button(L("wa.pair_number_ask")) { model.askCode() }
                        .disabled(model.state?.canAsk != true || model.number.allSatisfy { !$0.isNumber })
                    if let code = model.state?.pairingCode {
                        Text(code).font(.title.monospaced().bold()).textSelection(.enabled)
                            .frame(maxWidth: .infinity)
                        Text(L("wa.pair_code_help")).multilineTextAlignment(.center)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(.top, 8)
            }
            Text(L("wa.local")).font(.caption).foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Spacer()
                Button(L("actions.cancel")) { app.closeWhatsAppLink() }
                    .keyboardShortcut(.cancelAction)
            }
        }
        .padding(24)
    }

    /// The QR drawn module for module, on the white margin scanners need.
    static func image(_ code: String) -> NSImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(code.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage else { return nil }
        let margin = output.extent.insetBy(dx: -4, dy: -4)
        let framed = output.composited(over: CIImage(color: .white).cropped(to: margin))
        guard let cg = CIContext().createCGImage(framed, from: margin) else { return nil }
        return NSImage(cgImage: cg, size: NSSize(width: margin.width, height: margin.height))
    }
}
