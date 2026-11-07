import Foundation
import Observation
import RocketVibeCore

public protocol ThreadInboxSource: AnyObject, Sendable {
    func threadInbox(unread: Bool, cursor: String?) async throws -> ThreadInboxPage
    func threadTotals() async throws -> ThreadInboxTotals
    func markAllThreadsRead() async throws
    func threadInboxPolled() -> Bool
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
    public private(set) var scope = ThreadInboxScope.all
    public private(set) var entries: [ThreadInboxEntry] = []
    public private(set) var next: String?
    public private(set) var phase = Phase.loading
    public private(set) var totals = ThreadInboxTotals(unreadThreads: 0, unreadMentions: 0)
    public private(set) var loadingMore = false
    public private(set) var markingAll = false
    public var onNotice: ((String) -> Void)?

    @ObservationIgnored private let source: any ThreadInboxSource
    @ObservationIgnored private let refreshDelay: UInt64
    @ObservationIgnored private let pollInterval: UInt64
    @ObservationIgnored private var pages = 0
    @ObservationIgnored private var extraPages: UInt32 = 0
    @ObservationIgnored private var filling = 0
    @ObservationIgnored private var poll: Task<Void, Never>?
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var totalsGeneration = 0
    @ObservationIgnored private var refresh: Task<Void, Never>?

    public init(source: any ThreadInboxSource, refreshDelay: UInt64 = 400_000_000, pollInterval: UInt64 = 30_000_000_000) {
        self.source = source
        self.refreshDelay = refreshDelay
        self.pollInterval = pollInterval
    }

    public var visible: [ThreadInboxEntry] {
        entries.filter { threadInboxScopeMatches(scope: scope, participated: $0.participated, mentioned: $0.mentioned) }
    }

    public var emptyTitle: String {
        switch scope {
        case .all: unreadOnly ? "thread_inbox.none_unread" : "thread_inbox.none_followed"
        case .participated: "thread_inbox.none_participated"
        case .mentioned: "thread_inbox.none_mentioned"
        case .others: "thread_inbox.none_others"
        }
    }

    public var caughtUp: Bool { unreadOnly && scope == .all }

    public var badge: String? {
        let count = totals.unreadThreads
        return count > 0 ? (count > 99 ? "99+" : String(count)) : nil
    }

    public var mentioned: Bool { totals.unreadMentions > 0 }

    public func open() async {
        guard !shown else { return }
        shown = true
        startPolling()
        await reload()
    }

    public func close() {
        shown = false
        poll?.cancel()
        poll = nil
    }

    public func appBecameActive() {
        if source.threadInboxPolled() { scheduleRefresh() }
    }

    public func setScope(_ scope: ThreadInboxScope) async {
        guard scope != self.scope else { return }
        self.scope = scope
        extraPages = 0
        guard phase == .list || phase == .empty else { return }
        settle()
        await fill()
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
        extraPages = 0
        phase = .loading
        if await load(cursor: nil) { await fill() }
    }

    public func loadMore() async {
        guard let next, !loadingMore else { return }
        loadingMore = true
        extraPages = 0
        if await load(cursor: next) { await fill() }
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
        if shown { await readAgain() }
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
            if self.shown, self.pages <= 1 { await self.readAgain() }
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
        poll?.cancel()
        poll = nil
        filling &+= 1
        generation &+= 1
        totalsGeneration &+= 1
    }

    private func refreshTotals() async {
        totalsGeneration &+= 1
        let expected = totalsGeneration
        guard let fresh = try? await source.threadTotals(), expected == totalsGeneration else { return }
        if fresh != totals { totals = fresh }
    }

    private func startPolling() {
        poll?.cancel()
        guard source.threadInboxPolled() else { return }
        let interval = pollInterval
        poll = Task { [weak self] in
            while true {
                try? await Task.sleep(nanoseconds: interval)
                guard !Task.isCancelled, let self, self.shown else { return }
                self.scheduleRefresh()
            }
        }
    }

    private func readAgain() async {
        extraPages = 0
        if await load(cursor: nil) { await fill() }
    }

    /// Where the chip leaves the list short, the next pages are read, at most
    /// four per user action; past that the load-more button stays.
    private func fill() async {
        filling &+= 1
        let expected = filling
        while expected == filling,
              threadInboxKeepFetching(shown: UInt32(visible.count), hasNext: next != nil, extraPages: extraPages) {
            extraPages += 1
            loadingMore = true
            if visible.isEmpty { phase = .loading }
            guard await load(cursor: next) else { return }
        }
    }

    private func settle() {
        phase = visible.isEmpty && next == nil ? .empty : .list
    }

    @discardableResult
    private func load(cursor: String?) async -> Bool {
        generation &+= 1
        let expected = generation
        do {
            let page = try await source.threadInbox(unread: unreadOnly, cursor: cursor)
            guard expected == generation else { return false }
            loadingMore = false
            var seen = cursor == nil ? Set<String>() : Set(entries.map(\.root))
            let fresh = page.entries.filter { seen.insert($0.root).inserted }
            if cursor == nil {
                entries = fresh
                pages = 1
            } else {
                entries += fresh
                pages += 1
            }
            next = page.next
            settle()
            return true
        } catch {
            guard expected == generation else { return false }
            loadingMore = false
            if entries.isEmpty {
                phase = .failed
            } else {
                settle()
                onNotice?(L("info.failed"))
            }
            return false
        }
    }
}
