import assert from "node:assert/strict";
import test from "node:test";
import {
  appendPage,
  countLabel,
  inboxAddress,
  inboxUrl,
  refreshHead,
  refreshLimit,
} from "../src/thread-inbox.ts";
import type { ThreadInboxEntry } from "../src/protocol.ts";

const entry = (id: string, position: string) =>
  ({
    root: { id },
    last_reply_position: position,
  }) as unknown as ThreadInboxEntry;
const ids = (rows: ThreadInboxEntry[]) => rows.map((row) => row.root.id);

test("the address says the tab and the thread, and nothing else counts", () => {
  assert.deepEqual(inboxAddress(""), { tab: undefined, thread: undefined });
  assert.deepEqual(inboxAddress("?notes=1"), {
    tab: undefined,
    thread: undefined,
  });
  assert.equal(inboxAddress("?threads=unread").tab, "unread");
  assert.equal(inboxAddress("?threads=all").tab, undefined);
  assert.deepEqual(inboxAddress("?threads=followed&thread=root-1"), {
    tab: "followed",
    thread: "root-1",
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
