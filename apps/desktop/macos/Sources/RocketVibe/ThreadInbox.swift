import RocketVibeCore
import RocketVibeKit
import SwiftUI

private let shownParticipants = 3

private struct ThreadCountBadge: View {
    let text: String
    let mention: Bool

    var body: some View {
        Text(text)
            .font(.vibe(11.5, .heavy))
            .padding(.horizontal, 7)
            .frame(minWidth: 22, minHeight: 20)
            .background(mention ? Vibe.pink : Vibe.sun, in: Capsule())
            .foregroundStyle(Vibe.ink)
    }
}

/// The room list's way into the account's threads.
struct ThreadInboxEntryRow: View {
    let inbox: ThreadInboxModel

    var body: some View {
        Button { Task { await inbox.open() } } label: {
            HStack(spacing: 10) {
                Image(systemName: "bubble.left.and.bubble.right")
                    .frame(width: 36)
                    .foregroundStyle(Vibe.soft)
                Text(L("threads.title"))
                    .font(.vibe(14.5, inbox.badge == nil ? .bold : .heavy))
                    .foregroundStyle(inbox.badge == nil ? Vibe.soft : Vibe.text)
                    .lineLimit(1)
                Spacer()
                if let badge = inbox.badge { ThreadCountBadge(text: badge, mention: inbox.mentioned) }
            }
            .padding(.vertical, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// The account's threads in place of the room list; one picked opens beside it.
struct ThreadInboxPanel: View {
    @Environment(AppModel.self) var app
    let inbox: ThreadInboxModel

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                Button { inbox.close() } label: { Image(systemName: "chevron.left") }
                    .buttonStyle(.borderless)
                    .help(L("thread_inbox.back"))
                Text(L("threads.title"))
                    .font(.vibeTitle(17, .bold))
                    .lineLimit(1)
                Spacer()
                Button { Task { await inbox.markAllRead() } } label: { Image(systemName: "checkmark.circle") }
                    .buttonStyle(.borderless)
                    .help(L("thread_inbox.mark_all"))
                    .disabled(inbox.totals.unreadThreads == 0 || inbox.markingAll)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
            Picker("", selection: Binding(
                get: { inbox.unreadOnly },
                set: { on in Task { await inbox.setUnreadOnly(on) } }
            )) {
                Text(L("thread_inbox.followed")).tag(false)
                Text(L("thread_inbox.unread")).tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, 14)
            .padding(.bottom, 6)
            content
        }
    }

    @ViewBuilder var content: some View {
        switch inbox.phase {
        case .loading:
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed:
            VStack(spacing: 10) {
                Image(systemName: "exclamationmark.triangle")
                    .font(.system(size: 26))
                    .foregroundStyle(Vibe.muted)
                Text(L("info.failed")).foregroundStyle(Vibe.muted)
                Button(L("native.retry")) { Task { await inbox.reload() } }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .empty:
            VStack(spacing: 10) {
                Image(systemName: inbox.unreadOnly ? "checkmark.circle" : "bubble.left.and.bubble.right")
                    .font(.system(size: 26))
                    .foregroundStyle(Vibe.muted)
                Text(L(inbox.unreadOnly ? "thread_inbox.none_unread" : "thread_inbox.none_followed"))
                    .font(.vibeTitle(15, .bold))
                    .multilineTextAlignment(.center)
                if inbox.unreadOnly {
                    Text(L("thread_inbox.caught_up")).foregroundStyle(Vibe.muted)
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .list:
            threads
        }
    }

    var threads: some View {
        let selection = Binding<String?>(
            get: { app.thread?.threadId },
            set: { root in
                if let entry = inbox.entries.first(where: { $0.root == root }) { app.openInboxThread(entry) }
            }
        )
        return List(selection: selection) {
            ForEach(inbox.entries, id: \.root) { entry in
                ThreadInboxRow(entry: entry).tag(entry.root)
            }
            if inbox.next != nil {
                Button(L("threads.more")) { Task { await inbox.loadMore() } }
                    .disabled(inbox.loadingMore)
                    .frame(maxWidth: .infinity)
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
    }
}

struct ThreadInboxRow: View {
    let entry: ThreadInboxEntry

    var unread: Bool { entry.unreadReplies > 0 }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(entry.author)
                    .font(.vibe(13.5, unread ? .heavy : .bold))
                    .foregroundStyle(unread ? Vibe.text : Vibe.soft)
                    .lineLimit(1)
                if !entry.room.isEmpty {
                    Text(entry.room)
                        .font(.vibe(11, .semibold))
                        .foregroundStyle(Vibe.soft)
                        .lineLimit(1)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 1)
                        .background(Vibe.raised, in: Capsule())
                }
                Spacer(minLength: 4)
                Text(Formatting.shortTime(entry.lastReply))
                    .font(.vibe(11, .semibold))
                    .foregroundStyle(unread ? Vibe.pinkSoft : Vibe.faint)
                    .fixedSize()
            }
            Text(entry.preview)
                .font(.vibe(12.5))
                .foregroundStyle(Vibe.muted)
                .lineLimit(2)
            HStack(spacing: 4) {
                ForEach(entry.participants.prefix(shownParticipants), id: \.username) { person in
                    Avatar(path: person.avatar, name: person.name, size: 18)
                }
                Text(L("message.replies", count: Int(entry.replies)))
                    .font(.vibe(11.5))
                    .foregroundStyle(Vibe.muted)
                    .padding(.leading, 4)
                Spacer()
                if unread {
                    ThreadCountBadge(text: String(entry.unreadReplies), mention: entry.unreadMentions > 0)
                }
            }
        }
        .padding(.vertical, 4)
    }
}
