import Foundation
import Observation
import RocketVibeCore

/// Linking a new WhatsApp device (rv-ffi `whatsapp.rs`): the QR the phone
/// scans, or a code asked by number, until the phone links it; closing the
/// sheet before that stops the attempt and erases its state.
@MainActor @Observable
public final class WhatsAppLinkModel {
    public private(set) var state: WhatsAppLinkState?
    public var number = ""
    @ObservationIgnored private var link: WhatsAppLink?
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var finished = false

    init() {}

    func start(client: Client, done: @escaping (WhatsAppChat) -> Void) {
        task = Task { [weak self] in
            guard let link = try? await client.whatsappLink() else {
                self?.state = WhatsAppLinkState(qr: nil, pairingCode: nil, status: L("wa.start_failed"),
                                                retry: false, canAsk: false, linked: false, over: true)
                return
            }
            guard let self, !Task.isCancelled else { await link.cancel(); return }
            self.link = link
            while !Task.isCancelled {
                let now = link.state()
                state = now
                if now.linked {
                    guard let chat = try? await client.whatsappFinish(link: link) else { return }
                    guard !Task.isCancelled else { chat.shutdown(); return }
                    finished = true
                    done(chat)
                    return
                }
                if now.over { return }
                await link.changed(limitMs: 30_000)
            }
        }
    }

    public func askCode() {
        guard let link, link.pairPhone(number: number) else { return }
        state = link.state()
    }

    public func retry() { link?.reconnect() }

    /// The sheet closed: an unlinked attempt goes.
    func stop() {
        task?.cancel()
        task = nil
        guard !finished, let link else { return }
        self.link = nil
        Task { await link.cancel() }
    }
}

extension AppModel {
    public func openWhatsAppLink() {
        guard whatsAppLink == nil else { return }
        let model = WhatsAppLinkModel()
        whatsAppLink = model
        model.start(client: client) { [weak self, weak model] chat in
            guard let self, let model, self.whatsAppLink === model else { chat.shutdown(); return }
            self.whatsAppLink = nil
            Task { await self.adopt(.whatsapp(chat)) }
        }
    }

    public func closeWhatsAppLink() {
        whatsAppLink?.stop()
        whatsAppLink = nil
    }
}
