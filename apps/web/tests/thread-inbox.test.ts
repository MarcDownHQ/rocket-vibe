import assert from "node:assert/strict";
import test from "node:test";
import {
  appendPage,
  countLabel,
  inboxAddress,
  inboxUrl,
  matchesFilter,
  refreshHead,
  refreshLimit,
  threadTraits,
} from "../src/thread-inbox.ts";
import type { ThreadInboxEntry } from "../src/protocol.ts";

const entry = (id: string, position: string) =>
  ({
    root: { id },
    last_reply_position: position,
  }) as unknown as ThreadInboxEntry;
const ids = (rows: ThreadInboxEntry[]) => rows.map((row) => row.root.id);

test("the address says the tab and the thread, and nothing else counts", () => {
  assert.deepEqual(inboxAddress(""), {
    tab: undefined,
    thread: undefined,
    filter: "all",
  });
  assert.deepEqual(inboxAddress("?notes=1"), {
    tab: undefined,
    thread: undefined,
    filter: "all",
  });
  assert.equal(inboxAddress("?threads=unread").tab, "unread");
  assert.equal(inboxAddress("?threads=all").tab, undefined);
  assert.deepEqual(inboxAddress("?threads=followed&thread=root-1"), {
    tab: "followed",
    thread: "root-1",
    filter: "all",
  });
  assert.equal(inboxUrl(undefined, undefined), "/");
  assert.equal(inboxUrl(undefined, "unread"), "/?threads=unread");
  assert.equal(
    inboxUrl("a room", "followed", "root-1"),
    "/room/a%20room?threads=followed&thread=root-1",
  );
  assert.equal(inboxUrl("room", undefined), "/room/room");
  assert.equal(inboxUrl("room", undefined, "r"), "/room/room?thread=r");
});

test("the address carries a chip beside its tab, and only there", () => {
  assert.equal(
    inboxAddress("?threads=unread&threadsFilter=mentioned").filter,
    "mentioned",
  );
  assert.equal(
    inboxAddress("?threads=unread&threadsFilter=nope").filter,
    "all",
  );
  assert.equal(inboxAddress("?threadsFilter=mentioned").filter, "all");
  assert.equal(
    inboxUrl("room", "unread", "root-1", "others"),
    "/room/room?threads=unread&threadsFilter=others&thread=root-1",
  );
  assert.equal(
    inboxUrl(undefined, "followed", undefined, "all"),
    "/?threads=followed",
  );
  assert.equal(inboxUrl("room", undefined, undefined, "others"), "/room/room");
});

const me = { id: "me", username: "alice" };
const thread = ({
  author = "bob",
  repliers = ["bob"],
  mentions = "0",
  text = "plain",
  personal = false,
} = {}) =>
  ({
    root: {
      author: { id: author },
      text,
      ...(personal ? { personal_mention: true } : {}),
    },
    participants: repliers.map((id) => ({ id })),
    mentions,
  }) as unknown as ThreadInboxEntry;

test("participated: my root, my reply, or a direct conversation", () => {
  const participated = (entry: ThreadInboxEntry, kind?: "direct" | "public") =>
    threadTraits(entry, me, kind).participated;
  assert.equal(participated(thread(), "public"), false);
  assert.equal(participated(thread(), undefined), false);
  assert.equal(participated(thread({ author: "me" }), "public"), true);
  assert.equal(
    participated(thread({ repliers: ["bob", "me"] }), "public"),
    true,
  );
  assert.equal(participated(thread(), "direct"), true);
});

test("mentioned: an unread mention, or the root naming me", () => {
  const mentioned = (entry: ThreadInboxEntry) =>
    threadTraits(entry, me, "public").mentioned;
  assert.equal(mentioned(thread()), false);
  assert.equal(mentioned(thread({ mentions: "2" })), true);
  assert.equal(mentioned(thread({ text: "ask @alice first" })), true);
  assert.equal(mentioned(thread({ text: "ask @alicia first" })), false);
  assert.equal(mentioned(thread({ personal: true })), true);
});

test("the chips: two overlap, Others is neither, All is everything", () => {
  const cases = [
    [false, false, { participated: false, mentioned: false, others: true }],
    [true, false, { participated: true, mentioned: false, others: false }],
    [false, true, { participated: false, mentioned: true, others: false }],
    [true, true, { participated: true, mentioned: true, others: false }],
  ] as const;
  for (const [participated, mentioned, expected] of cases) {
    const traits = { participated, mentioned };
    assert.equal(matchesFilter("all", traits), true);
    assert.equal(matchesFilter("participated", traits), expected.participated);
    assert.equal(matchesFilter("mentioned", traits), expected.mentioned);
    assert.equal(matchesFilter("others", traits), expected.others);
  }
});

test("a later page keeps the order and shows a moved thread once", () => {
  assert.deepEqual(
    ids(
      appendPage(
        [entry("a", "90"), entry("b", "80")],
        [entry("c", "70"), entry("a", "60")],
      ),
    ),
    ["b", "c", "a"],
  );
  // Past 2^53: compared exactly.
  assert.deepEqual(
    ids(
      appendPage(
        [entry("low", "9007199254740993")],
        [entry("high", "9007199254740995")],
      ),
    ),
    ["high", "low"],
  );
});

test("reading the first page again replaces what it covers", () => {
  const shown = [entry("a", "90"), entry("b", "80"), entry("c", "70")];
  assert.deepEqual(ids(refreshHead(shown, [entry("c", "95")], true)), ["c"]);
  assert.deepEqual(
    ids(refreshHead(shown, [entry("c", "95"), entry("a", "90")], false)),
    ["c", "a", "b"],
  );
  assert.deepEqual(refreshHead(shown, [], true), []);
  assert.deepEqual(refreshHead(shown, [], false), []);
  assert.equal(refreshLimit(3), 50);
  assert.equal(refreshLimit(70), 70);
  assert.equal(refreshLimit(400), 100);
});

test("a large count reads 99+", () => {
  assert.equal(countLabel("7"), "7");
  assert.equal(countLabel("99"), "99");
  assert.equal(countLabel("100"), "99+");
});
