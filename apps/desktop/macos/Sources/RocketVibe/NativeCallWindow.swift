import AppKit
import RocketVibeCore
import RocketVibeKit
import SwiftUI

/// A call carried by the voice sidecar (Jitsi Meet, kMeet, a huddle, Teams,
/// Mattermost), as GTK's `native_call.rs`, drawn with the Vibez voice page's
/// parts: a tile per person with the speaking ring, their camera filling it, a
/// shared screen on a stage with the people beside it, and the same control bar. A lost call shows "Reconnecting…" while the sidecar joins again;
/// one that ends stays on screen with the reason. `CallWindow.show` tries it
/// first and keeps the meeting's page for anything else.
@MainActor
final class NativeCallWindow: NSObject, NSWindowDelegate {
    private static var windows: [NativeCallWindow] = []
    private let window: NSWindow
    private let call: JitsiCallSession

    /// `chat`, the call's room, goes beside the call behind a button.
    static func show(_ call: JitsiCallSession, title: String, chat: RoomModel? = nil, app: AppModel? = nil) {
        let native = NativeCallWindow(call: call, title: title, chat: chat, app: app)
        windows.append(native)
        native.window.makeKeyAndOrderFront(nil)
    }

    private init(call: JitsiCallSession, title: String, chat: RoomModel?, app: AppModel?) {
        self.call = call
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 720, height: 600),
                          styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        super.init()
        window.title = title
        window.isReleasedWhenClosed = false
        window.center()
        window.delegate = self
        let view = NativeCallView(call: call, title: title, chat: app == nil ? nil : chat,
                                  close: { [weak self] in self?.window.close() })
        if let app {
            window.contentView = NSHostingView(rootView: view.environment(app))
        } else {
            window.contentView = NSHostingView(rootView: view)
        }
    }

    func windowWillClose(_ notification: Notification) {
        let call = self.call
        Task { await call.leave() }
        Self.windows.removeAll { $0 === self }
    }
}

private struct NativeCallView: View {
    let call: JitsiCallSession
    let title: String
    /// The call's room, its chat shown beside the call on demand.
    let chat: RoomModel?
    let close: () -> Void
    @State private var state: JitsiCallState?
    @State private var connectedAt: Date?
    @State private var chatShown = false
    @State private var staged: [URL] = []
    private let tick = Timer.publish(every: 0.3, on: .main, in: .common).autoconnect()

    var body: some View {
        HStack(spacing: 0) {
            callBody
            if chatShown, let chat {
                Divider()
                VStack(spacing: 0) {
                    MessageList(model: chat)
                    if !chat.writingLocked { Composer(model: chat, staged: $staged) }
                }
                .frame(width: 340)
                .background(Vibe.night)
            }
        }
    }

    private var callBody: some View {
        VStack(spacing: 14) {
            HStack(spacing: 8) {
                Image(systemName: "phone.fill")
                Text(title).font(.vibe(17, .heavy)).lineLimit(1)
                if chat != nil {
                    Spacer()
                    Button { chatShown.toggle() } label: { Image(systemName: "bubble.left.and.bubble.right") }
                        .buttonStyle(.borderless)
                        .help(L("voice_session.open_chat"))
                }
            }
            Text(status)
                .font(.vibe(13, .bold))
                .foregroundStyle(state?.connected == true && state?.ended != true ? Vibe.mint : Vibe.muted)
            if state?.ended == true {
                Spacer()
                Button(L("call.close"), action: close).buttonStyle(.borderedProminent)
                Spacer()
            } else {
                let members = state?.members ?? []
                // Someone else's screen before this side's own, which it sees anyway.
                if let sharer = members.first(where: { $0.screen && !$0.local }) ?? members.first(where: \.screen) {
                    HStack(spacing: 12) {
                        ZStack {
                            RoundedRectangle(cornerRadius: 20).fill(Vibe.night)
                            VoiceVideo(call: call, member: sharer, screen: true)
                                .clipShape(RoundedRectangle(cornerRadius: 18))
                        }
                        .overlay(RoundedRectangle(cornerRadius: 20).strokeBorder(Vibe.line, lineWidth: 2))
                        ScrollView {
                            VStack(spacing: 10) {
                                ForEach(members, id: \.id) { member in
                                    tile(member).frame(width: 176, height: 99)
                                }
                            }
                        }
                        .frame(width: 176)
                    }
                } else {
                    GeometryReader { geometry in
                        let arranged = arrangeTiles(members.count, in: geometry.size)
                        let columns = max(1, arranged.columns), tile = arranged.tile
                        let rows = stride(from: 0, to: members.count, by: columns)
                            .map { Array(members[$0..<min($0 + columns, members.count)]) }
                        VStack(spacing: 12) {
                            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                                HStack(spacing: 12) {
                                    ForEach(row, id: \.id) { member in
                                        self.tile(member).frame(width: tile.width, height: tile.height)
                                    }
                                }
                            }
                        }
                        .frame(width: geometry.size.width, height: geometry.size.height)
                    }
                }
                HStack(spacing: 14) {
                    let muted = !(state?.microphone ?? true)
                    let deafened = state?.deafened ?? false
                    VoiceControlButton(icon: muted ? "mic.slash.fill" : "mic.fill", off: muted,
                                       help: muted ? "voice_session.unmute" : "voice_session.mute") {
                        Task { await call.setMicrophone(enabled: muted) }
                    }
                    VoiceControlButton(icon: deafened ? "speaker.slash.fill" : "headphones", off: deafened,
                                       help: deafened ? "voice_session.undeafen" : "voice_session.deafen") {
                        Task { await call.setDeafened(deafened: !deafened) }
                    }
                    // No camera button: the sidecar captures none on macOS, as on the voice page.
                    if state?.canScreen == true {
                        let sharing = state?.sharing ?? false
                        VoiceControlButton(icon: "rectangle.inset.filled.on.rectangle", on: sharing,
                                           help: sharing ? "voice_session.stop_screen" : "voice_session.share_screen") {
                            Task { if sharing { await call.stopScreenShare() } else { await call.startScreenShare() } }
                        }
                    }
                    VoiceHangUpButton(help: "call.hang_up", action: close)
                }
            }
        }
        .padding(18)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Vibe.ink)
        .onReceive(tick) { _ in
            let next = call.state()
            if next.connected && connectedAt == nil { connectedAt = Date() }
            state = next
            // Hung up here: nothing to show. Lost for good: the reason stays until closed.
            if next.ended && next.endedReason == nil { close() }
        }
    }

    /// Someone: their camera filling the tile, else their initial.
    private func tile(_ member: VoiceMember) -> some View {
        VoiceTileFace(member: member, named: true) {
            if member.camera {
                VoiceVideo(call: call, member: member, fill: true)
                    .clipShape(RoundedRectangle(cornerRadius: 16))
            } else {
                SpeakingAvatar(member: member, size: 96)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }

    private var status: String {
        if let reason = state?.endedReason { return L("call.ended", ["reason": reason]) }
        if state?.reconnecting == true { return L("voice_session.reconnecting") }
        guard state?.connected == true, let at = connectedAt else { return L("call.connecting") }
        if (state?.members ?? []).allSatisfy(\.local) { return L("call.waiting") }
        let seconds = Int(Date().timeIntervalSince(at))
        return String(format: "%d:%02d", seconds / 60, seconds % 60)
    }
}
