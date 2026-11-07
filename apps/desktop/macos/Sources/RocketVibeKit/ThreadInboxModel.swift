import Foundation
import Observation
import RocketVibeCore

public protocol ThreadInboxSource: AnyObject, Sendable {
    func threadInbox(unread: Bool, cursor: String?) async throws -> ThreadInboxPage
    func threadTotals() async throws -> ThreadInboxTotals
    func markAllThreadsRead() async throws
}

extension Chat: ThreadInboxSource {}

/// The account's threads, in place of the room list: those I follow, those
/// with replies I have not read, and the totals of the room list's row.
@MainActor @Observable
public final class ThreadInboxModel {
    public enum Phase: Equatable {
        case loading, list, empty, failed
    }

    public private(set) var shown = false
    public private(set) var unreadOnly = false
    public private(set) var entries: [ThreadInboxEntry] = []
    public private(set) var next: String?
    public private(set) var phase = Phase.loading
    public private(set) var totals = ThreadInboxTotals(unreadThreads: 0, unreadMentions: 0)
    public private(set) var loadingMore = false
    public private(set) var markingAll = false
    public var onNotice: ((String) -> Void)?

    @ObservationIgnored private let source: any ThreadInboxSource
    @ObservationIgnored private let refreshDelay: UInt64
    @ObservationIgnored private var pages = 0
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var totalsGeneration = 0
    @ObservationIgnored private var refresh: Task<Void, Never>?

    public init(source: any ThreadInboxSource, refreshDelay: UInt64 = 400_000_000) {
        self.source = source
        self.refreshDelay = refreshDelay
    }

    public var badge: String? {
        let count = totals.unreadThreads
        return count > 0 ? (count > 99 ? "99+" : String(count)) : nil
    }

    public var mentioned: Bool { totals.unreadMentions > 0 }

    public func open() async {
        guard !shown else { return }
        shown = true
        await reload()
    }

    public func close() {
        shown = false
    }

    public func setUnreadOnly(_ on: Bool) async {
        guard on != unreadOnly else { return }
        unreadOnly = on
        await reload()
    }

    public func reload() async {
        entries = []
        next = nil
        pages = 0
        phase = .loading
        await load(cursor: nil)
    }

    public func loadMore() async {
        guard let next, !loadingMore else { return }
        loadingMore = true
        await load(cursor: next)
    }

    public func markAllRead() async {
        guard !markingAll else { return }
        markingAll = true
        defer { markingAll = false }
        do {
            try await source.markAllThreadsRead()
        } catch {
            onNotice?(L("info.failed"))
        }
        await refreshTotals()
        if shown { await load(cursor: nil) }
    }

    /// Something changed on the server: a burst of events reads once. A list
    /// read further than its first page stays as it is: reading it again would
    /// throw the reader back to the top.
    public func scheduleRefresh() {
        refresh?.cancel()
        let delay = refreshDelay
        refresh = Task { [weak self] in
            try? await Task.sleep(nanoseconds: delay)
            guard !Task.isCancelled, let self else { return }
            await self.refreshTotals()
            if self.shown, self.pages <= 1 { await self.load(cursor: nil) }
        }
    }

    /// The thread was just read here: its row says so before the server does.
    public func noteRead(_ root: String) {
        guard let at = entries.firstIndex(where: { $0.root == root }) else { return }
        entries[at].unreadReplies = 0
        entries[at].unreadMentions = 0
    }

    public func stop() {
        refresh?.cancel()
        refresh = nil
        generation &+= 1
        totalsGeneration &+= 1
    }

    private func refreshTotals() async {
        totalsGeneration &+= 1
        let expected = totalsGeneration
        guard let fresh = try? await source.threadTotals(), expected == totalsGeneration else { return }
        if fresh != totals { totals = fresh }
    }

    private func load(cursor: String?) async {
        generation &+= 1
        let expected = generation
        do {
            let page = try await source.threadInbox(unread: unreadOnly, cursor: cursor)
            guard expected == generation else { return }
            loadingMore = false
            if cursor == nil {
                entries = page.entries
                pages = 1
            } else {
                let known = Set(entries.map(\.root))
                entries += page.entries.filter { !known.contains($0.root) }
                pages += 1
            }
            next = page.next
            phase = entries.isEmpty ? .empty : .list
        } catch {
            guard expected == generation else { return }
            loadingMore = false
            if entries.isEmpty {
                phase = .failed
            } else {
                onNotice?(L("info.failed"))
            }
        }
    }
}
