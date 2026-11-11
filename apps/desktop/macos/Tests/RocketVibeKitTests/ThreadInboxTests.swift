import Foundation
import RocketVibeCore
import XCTest

@testable import RocketVibeKit

private func entry(_ root: String, unread: Int64 = 0, participated: Bool = false, mentioned: Bool = false) -> ThreadInboxEntry {
    ThreadInboxEntry(
        root: root, rid: "room", author: "ann", preview: "hello", room: "general", lastReply: 1, replies: 2,
        participants: [], unreadReplies: unread, unreadMentions: unread, participated: participated, mentioned: mentioned
    )
}

private func full(_ prefix: String) -> [String] {
    (0..<12).map { "\(prefix)\($0)" }
}

@MainActor
private final class FakeSource: ThreadInboxSource, @unchecked Sendable {
    var pages: [String: ThreadInboxPage] = [:]
    var totals = ThreadInboxTotals(unreadThreads: 0, unreadMentions: 0)
    var failing = false
    var asked: [(unread: Bool, cursor: String?)] = []
    var totalsAsked = 0
    var markedAll = 0
    var held: CheckedContinuation<Void, Never>?
    var holdNext = false
    var polled = false

    private static func key(_ unread: Bool, _ cursor: String?) -> String { "\(unread)/\(cursor ?? "")" }

    func set(unread: Bool = false, cursor: String? = nil, _ roots: [String], next: String? = nil) {
        pages[Self.key(unread, cursor)] = ThreadInboxPage(entries: roots.map { entry($0) }, next: next)
    }

    func threadInbox(unread: Bool, cursor: String?) async throws -> ThreadInboxPage {
        asked.append((unread, cursor))
        let page = pages[Self.key(unread, cursor)]
        if holdNext {
            holdNext = false
            await withCheckedContinuation { held = $0 }
        }
        if failing { throw RvError.Local(message: "down") }
        return page ?? ThreadInboxPage(entries: [], next: nil)
    }

    func threadTotals() async throws -> ThreadInboxTotals {
        totalsAsked += 1
        if failing { throw RvError.Local(message: "down") }
        return totals
    }

    nonisolated func threadInboxPolled() -> Bool {
        MainActor.assumeIsolated { polled }
    }

    func markAllThreadsRead() async throws {
        markedAll += 1
        totals = ThreadInboxTotals(unreadThreads: 0, unreadMentions: 0)
        pages = [:]
    }
}

@MainActor
final class ThreadInboxTests: XCTestCase {
    private func until(_ what: @autoclosure () -> Bool, _ message: String = "") async {
        for _ in 0..<400 where !what() {
            try? await Task.sleep(nanoseconds: 5_000_000)
        }
        XCTAssertTrue(what(), message)
    }

    func testABurstOfEventsReadsOnce() async {
        let source = FakeSource()
        source.set(["a"])
        let inbox = ThreadInboxModel(source: source, refreshDelay: 20_000_000)
        await inbox.open()
        XCTAssertEqual(source.asked.count, 1)
        for _ in 0..<5 { inbox.scheduleRefresh() }
        await until(source.totalsAsked > 0)
        await until(source.asked.count > 1)
        try? await Task.sleep(nanoseconds: 60_000_000)
        XCTAssertEqual(source.totalsAsked, 1)
        XCTAssertEqual(source.asked.count, 2)
    }

    func testClosedPanelReadsOnlyTheTotals() async {
        let source = FakeSource()
        source.totals = ThreadInboxTotals(unreadThreads: 120, unreadMentions: 1)
        let inbox = ThreadInboxModel(source: source, refreshDelay: 1_000_000)
        XCTAssertNil(inbox.badge)
        inbox.scheduleRefresh()
        await until(inbox.badge != nil)
        XCTAssertEqual(inbox.badge, "99+")
        XCTAssertTrue(inbox.mentioned)
        XCTAssertTrue(source.asked.isEmpty)
    }

    func testAListReadFurtherStaysWhereItIs() async {
        let source = FakeSource()
        source.set(full("a"), next: "2")
        source.set(cursor: "2", ["a11", "c"])
        let inbox = ThreadInboxModel(source: source, refreshDelay: 1_000_000)
        await inbox.open()
        XCTAssertEqual(inbox.next, "2", "a full page reads no further by itself")
        await inbox.loadMore()
        XCTAssertEqual(inbox.entries.map(\.root), full("a") + ["c"], "a thread on both pages shows once")
        XCTAssertNil(inbox.next)
        let asked = source.asked.count
        inbox.scheduleRefresh()
        await until(source.totalsAsked > 0)
        try? await Task.sleep(nanoseconds: 20_000_000)
        XCTAssertEqual(source.asked.count, asked)
    }

    func testPagesReadByThemselvesStillRefresh() async {
        let source = FakeSource()
        source.set(["a"], next: "2")
        source.set(cursor: "2", ["b"])
        let inbox = ThreadInboxModel(source: source, refreshDelay: 1_000_000)
        await inbox.open()
        XCTAssertEqual(source.asked.map(\.cursor), [nil, "2"], "a short list reads on by itself")
        source.set(["a", "c"])
        inbox.scheduleRefresh()
        await until(inbox.entries.map(\.root) == ["a", "c"], "a refresh reads the list again")
    }

    func testAnAnswerForTheOtherTabIsDropped() async {
        let source = FakeSource()
        source.set(["followed"])
        source.set(unread: true, ["unread"])
        source.holdNext = true
        let inbox = ThreadInboxModel(source: source)
        let opening = Task { await inbox.open() }
        await until(source.held != nil)
        await inbox.setUnreadOnly(true)
        XCTAssertEqual(inbox.entries.map(\.root), ["unread"])
        source.held?.resume()
        await opening.value
        XCTAssertEqual(inbox.entries.map(\.root), ["unread"])
        XCTAssertEqual(inbox.phase, .list)
    }

    func testFailuresAndEmptyStates() async {
        let source = FakeSource()
        source.failing = true
        var notices: [String] = []
        let inbox = ThreadInboxModel(source: source)
        inbox.onNotice = { notices.append($0) }
        await inbox.open()
        XCTAssertEqual(inbox.phase, .failed)
        XCTAssertTrue(notices.isEmpty, "the panel itself says it failed")
        source.failing = false
        await inbox.reload()
        XCTAssertEqual(inbox.phase, .empty)
        source.set(full("a"), next: "2")
        await inbox.reload()
        XCTAssertEqual(inbox.phase, .list)
        source.failing = true
        await inbox.loadMore()
        XCTAssertEqual(inbox.phase, .list)
        XCTAssertEqual(inbox.entries.map(\.root), full("a"))
        XCTAssertEqual(notices.count, 1)
        XCTAssertFalse(inbox.loadingMore)
    }

    func testMarkAllReadsTheListAgainAndARowReadHereClearsAtOnce() async {
        let source = FakeSource()
        source.totals = ThreadInboxTotals(unreadThreads: 2, unreadMentions: 0)
        source.pages["true/"] = ThreadInboxPage(entries: [entry("a", unread: 3), entry("b", unread: 1)], next: "2")
        let inbox = ThreadInboxModel(source: source)
        await inbox.open()
        await inbox.setUnreadOnly(true)
        inbox.noteRead("a")
        XCTAssertEqual(inbox.entries.map(\.unreadReplies), [0, 1])
        await inbox.loadMore()
        await inbox.markAllRead()
        XCTAssertEqual(source.markedAll, 1)
        XCTAssertEqual(inbox.phase, .empty)
        XCTAssertNil(inbox.badge)
        XCTAssertFalse(inbox.markingAll)
    }

    func testChipsKeepTheCoreRuleAndEachHasItsEmptyPage() async {
        let source = FakeSource()
        source.pages["false/"] = ThreadInboxPage(entries: [
            entry("mine", participated: true),
            entry("named", mentioned: true),
            entry("both", participated: true, mentioned: true),
            entry("other"),
        ], next: nil)
        source.pages["true/"] = ThreadInboxPage(entries: [entry("named", mentioned: true)], next: nil)
        let inbox = ThreadInboxModel(source: source)
        await inbox.open()
        XCTAssertEqual(inbox.scope, .all)
        XCTAssertEqual(inbox.visible.map(\.root), ["mine", "named", "both", "other"])
        await inbox.setScope(.participated)
        XCTAssertEqual(inbox.visible.map(\.root), ["mine", "both"])
        await inbox.setScope(.mentioned)
        XCTAssertEqual(inbox.visible.map(\.root), ["named", "both"], "participated and mentioned overlap")
        await inbox.setScope(.others)
        XCTAssertEqual(inbox.visible.map(\.root), ["other"])
        XCTAssertEqual(source.asked.count, 1, "a chip filters what is loaded")

        await inbox.setUnreadOnly(true)
        XCTAssertEqual(inbox.scope, .others, "the chip stays across the tabs")
        XCTAssertEqual(inbox.phase, .empty)
        XCTAssertEqual(inbox.emptyTitle, "thread_inbox.none_others")
        XCTAssertFalse(inbox.caughtUp)
        await inbox.setScope(.participated)
        XCTAssertEqual(inbox.emptyTitle, "thread_inbox.none_participated")
        await inbox.setScope(.mentioned)
        XCTAssertEqual(inbox.phase, .list)
        await inbox.setScope(.all)
        XCTAssertEqual(inbox.emptyTitle, "thread_inbox.none_unread")
        XCTAssertTrue(inbox.caughtUp)
        await inbox.setUnreadOnly(false)
        XCTAssertEqual(inbox.emptyTitle, "thread_inbox.none_followed")
    }

    func testAShortListReadsAtMostFourMorePagesPerAction() async {
        let source = FakeSource()
        for page in 0..<20 {
            source.set(cursor: page == 0 ? nil : String(page), ["o\(page)"], next: String(page + 1))
        }
        let inbox = ThreadInboxModel(source: source)
        await inbox.open()
        XCTAssertEqual(source.asked.map(\.cursor), [nil, "1", "2", "3", "4"])
        XCTAssertEqual(inbox.visible.count, 5)

        await inbox.setScope(.mentioned)
        XCTAssertEqual(source.asked.count, 9, "the chip reads four more pages, no more")
        XCTAssertTrue(inbox.visible.isEmpty)
        XCTAssertEqual(inbox.phase, .list, "past the bound the list offers to load more")
        XCTAssertEqual(inbox.next, "9")
        XCTAssertFalse(inbox.loadingMore)

        await inbox.loadMore()
        XCTAssertEqual(source.asked.count, 14, "load more is a new action with its own bound")

        source.pages["false/14"] = ThreadInboxPage(entries: [entry("named", mentioned: true)], next: nil)
        await inbox.loadMore()
        XCTAssertEqual(source.asked.count, 15, "nothing left to read")
        XCTAssertEqual(inbox.visible.map(\.root), ["named"])
        XCTAssertNil(inbox.next)
    }

    func testAPolledSourceIsReadOnATimerAndOnReturning() async {
        let source = FakeSource()
        source.set(["a"])
        let still = ThreadInboxModel(source: source, refreshDelay: 1_000_000, pollInterval: 5_000_000)
        await still.open()
        still.appBecameActive()
        try? await Task.sleep(nanoseconds: 40_000_000)
        XCTAssertEqual(source.asked.count, 1, "a source with live events is not polled")
        still.stop()

        source.polled = true
        source.asked = []
        let inbox = ThreadInboxModel(source: source, refreshDelay: 1_000_000, pollInterval: 5_000_000)
        await inbox.open()
        await until(source.asked.count > 2, "read again while on screen")
        inbox.close()
        try? await Task.sleep(nanoseconds: 20_000_000)
        let asked = source.totalsAsked
        try? await Task.sleep(nanoseconds: 40_000_000)
        XCTAssertEqual(source.totalsAsked, asked, "closed, the timer stops")
        inbox.appBecameActive()
        await until(source.totalsAsked > asked, "returning to the app reads the totals")
        inbox.stop()
    }
}
