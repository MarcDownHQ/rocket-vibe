// The account's Threads view: the row and its badge, Followed and Unread across
// two rooms, opening a thread without reading its room, mark all, a room left,
// back and forward through the address, and the chips with their page bound.
import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";
const base = process.env.RV_WEB_TEST_URL || "http://127.0.0.1:3417";
assert.ok(["127.0.0.1", "localhost"].includes(new URL(base).hostname));
const username = process.env.RV_WEB_THREADS_USER || "webthreads";
const peerName = process.env.RV_WEB_THREADS_PEER || "webthreadspeer";
const password = "web-client-disposable-password";
const call = async (path, token, body, method) => {
  const response = await fetch(base + path, {
    method: method || (body === undefined ? "GET" : "POST"),
    headers: {
      ...(token ? { Authorization: "Bearer " + token } : {}),
      ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = response.status === 204 ? undefined : await response.json();
  assert.ok(
    response.ok,
    path + " " + response.status + " " + JSON.stringify(value),
  );
  return value;
};
const me = await call("/api/v1/auth/login", null, { username, password });
const peer = await call("/api/v1/auth/login", null, {
  username: peerName,
  password,
});
const discovery = await call("/.well-known/vibez");
assert.ok(discovery.capabilities.thread_inbox, "thread_inbox announced");
const newRoom = async (name) => {
  const room = await call("/api/v1/rooms", peer.token, {
    name: name + "-" + crypto.randomUUID().slice(0, 8),
    private: true,
    operation_id: crypto.randomUUID(),
  });
  await call(
    "/api/v1/rooms/" + room.id + "/members/" + me.user.id,
    peer.token,
    null,
  );
  return room;
};
const say = (token, room, text, root) =>
  call("/api/v1/rooms/" + room.id + "/messages", token, {
    operation_id: crypto.randomUUID(),
    text,
    ...(root ? { reply_to: root } : {}),
  });
const follow = (root) =>
  call(
    "/api/v1/messages/" + root.id + "/follow",
    me.token,
    { operation_id: crypto.randomUUID(), present: true },
    "PUT",
  );
const totals = () => call("/api/v1/threads/totals", me.token);
const roomRead = (room) => call("/api/v1/rooms/" + room.id + "/read", me.token);
// Earlier suites share these accounts: start from nothing unread.
await call("/api/v1/threads/read", me.token, {
  operation_id: crypto.randomUUID(),
});
assert.equal((await totals()).unread_threads, "0");

const alpha = await newRoom("inbox-alpha");
const beta = await newRoom("inbox-beta");
const gone = await newRoom("inbox-gone");
const first = await say(peer.token, alpha, "First root in alpha");
const second = await say(peer.token, beta, "Second root in beta");
const left = await say(peer.token, gone, "A root in a room I will leave");
for (const root of [first, second, left]) await follow(root);
await say(peer.token, alpha, "One reply", first.id);
await say(peer.token, gone, "One reply there", left.id);
await say(peer.token, beta, "@" + username + " a reply for you", second.id);

await mkdir("../../.cache/web-shots", { recursive: true });
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({
  locale: "en-US",
  viewport: { width: 1280, height: 800 },
});
const failures = [];
page.on("pageerror", (error) => failures.push(error.message));
const panel = page.locator(".threads-panel");
const rows = () =>
  panel
    .locator(".inbox-row")
    .evaluateAll((nodes) => nodes.map((node) => node.dataset.thread));
const row = (root) => panel.locator('[data-thread="' + root.id + '"]');
const badge = page.locator(".threads-row .badge");
const tab = (name) => panel.getByRole("tab", { name, exact: true });
const chip = (name) => panel.getByRole("button", { name, exact: true });
const waitFor = async (check, what) => {
  for (let tries = 0; tries < 60; tries++) {
    if (await check()) return;
    await page.waitForTimeout(250);
  }
  assert.fail("timed out: " + what);
};
try {
  await page.goto(base);
  await page.getByLabel("Username or email").fill(username);
  await page.getByLabel("Password", { exact: true }).fill(password);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.locator(".status-dot.online").waitFor();

  await badge.waitFor();
  assert.equal(await badge.textContent(), "3");
  assert.match(await badge.getAttribute("class"), /badge-mention/);
  console.log("PASS the row counts three unread threads, with a mention");

  await page.locator(".threads-row").click();
  await panel.waitFor();
  assert.equal(new URL(page.url()).searchParams.get("threads"), "followed");
  assert.ok(!(await page.locator(".threads-row").isVisible()));
  await row(second).waitFor();
  assert.deepEqual(
    (await rows()).slice(0, 3),
    [second.id, left.id, first.id],
    "latest reply first, across rooms",
  );
  const shown = row(second);
  assert.equal(await shown.locator(".inbox-author").textContent(), peerName);
  assert.equal(await shown.locator(".inbox-room").textContent(), beta.name);
  assert.equal(await shown.locator(".inbox-replies").textContent(), "1 reply");
  assert.equal(await shown.locator(".badge").textContent(), "1");
  assert.equal(await shown.locator(".tile").count(), 1);
  assert.match(await shown.locator(".inbox-preview").textContent(), /beta/);
  console.log("PASS Followed lists two rooms' threads, latest reply first");
  await page.screenshot({ path: "../../.cache/web-shots/thread-inbox.png" });

  await tab("Unread").click();
  await waitFor(
    async () =>
      new URL(page.url()).searchParams.get("threads") === "unread" &&
      (await rows()).length === 3,
    "three unread rows",
  );

  // A reply from the other account arrives live, in the list and the badge.
  await page.reload();
  await panel.waitFor();
  await row(first).waitFor();
  assert.equal(await tab("Unread").getAttribute("aria-selected"), "true");
  await say(peer.token, alpha, "Another reply", first.id);
  await waitFor(
    async () => (await row(first).locator(".badge").textContent()) === "2",
    "the live unread count",
  );
  assert.equal((await rows())[0], first.id);
  console.log(
    "PASS a new reply refreshes the row live; a reload keeps the tab",
  );

  const beforeRoom = (await roomRead(alpha)).root_position;
  await say(peer.token, alpha, "An unread root in alpha");
  const unreadRoots = (await roomRead(alpha)).unread_roots;
  assert.notEqual(unreadRoots, "0");
  await row(first).click();
  await page.locator(".thread-pane .thread-header").waitFor();
  await page.locator(".thread-pane").getByText("Another reply").waitFor();
  assert.ok(await panel.isVisible(), "the panel stays beside the thread");
  assert.ok(!(await page.locator(".room-content").isVisible()));
  let address = new URL(page.url());
  assert.equal(address.pathname, "/room/" + alpha.id);
  assert.equal(address.searchParams.get("thread"), first.id);
  await waitFor(
    async () => (await totals()).unread_threads === "2",
    "opening reads the thread",
  );
  await waitFor(
    async () => (await rows()).length === 2,
    "the row leaves Unread",
  );
  // Longer than the room's read delay: the room behind the thread stays unread.
  await page.waitForTimeout(2500);
  const room = await roomRead(alpha);
  assert.equal(room.root_position, beforeRoom);
  assert.equal(room.unread_roots, unreadRoots);
  console.log("PASS opening a row reads its thread, never its room");

  await page.goBack();
  await waitFor(
    async () => !(await page.locator(".thread-pane").isVisible()),
    "back closes the thread",
  );
  address = new URL(page.url());
  assert.equal(address.searchParams.get("thread"), null);
  assert.equal(address.searchParams.get("threads"), "unread");
  assert.ok(await panel.isVisible());
  await page.goForward();
  await page.locator(".thread-pane").getByText("Another reply").waitFor();
  assert.equal(new URL(page.url()).searchParams.get("thread"), first.id);
  await page.locator(".thread-pane .thread-back").click();
  await waitFor(
    async () => !new URL(page.url()).searchParams.get("thread"),
    "the thread's back button leaves the address",
  );
  assert.ok(await panel.isVisible());
  console.log("PASS back and forward walk the thread and the panel");

  // From one thread straight to another, in another room.
  await tab("Followed").click();
  await row(first).waitFor();
  await row(first).click();
  await page.locator(".thread-pane").getByText("Another reply").waitFor();
  await row(second).click();
  await page.locator(".thread-pane").getByText("a reply for you").waitFor();
  await panel
    .locator('.inbox-row.selected[data-thread="' + second.id + '"]')
    .waitFor();
  await page.goBack();
  await page.locator(".thread-pane").getByText("Another reply").waitFor();
  await panel
    .locator('.inbox-row.selected[data-thread="' + first.id + '"]')
    .waitFor();
  console.log("PASS one thread opens straight from another, across rooms");
  await waitFor(
    async () => (await totals()).unread_threads === "1",
    "the mention read",
  );

  // Reloaded on a thread: the same view, from the address alone.
  await page.reload();
  await page.locator(".thread-pane").getByText("Another reply").waitFor();
  await panel.waitFor();
  await page.locator(".thread-pane .thread-back").click();
  await page.locator(".thread-pane").waitFor({ state: "hidden" });
  await panel.locator(".inbox-row.selected").waitFor({ state: "detached" });
  console.log("PASS a reload on a thread restores it beside the panel");

  // A room I leave drops out of the list, of the totals and of mark all.
  const details = await call("/api/v1/rooms/" + gone.id, me.token);
  await call("/api/v1/rooms/" + gone.id + "/leave", me.token, {
    operation_id: crypto.randomUUID(),
    expected_revision: details.revision,
  });
  await waitFor(
    async () => !(await rows()).includes(left.id),
    "the room I left leaves the list",
  );
  await say(peer.token, gone, "Still talking there", left.id);
  await say(peer.token, alpha, "Back in alpha", first.id);
  await say(peer.token, beta, "@" + username + " back in beta", second.id);
  await waitFor(
    async () => (await totals()).unread_threads === "2",
    "the totals ignore the room I left",
  );
  assert.equal((await totals()).unread_mentions, "1");
  assert.ok(!(await rows()).includes(left.id));
  const listed = await call("/api/v1/threads?filter=followed", me.token);
  assert.ok(!listed.threads.some((entry) => entry.root.id === left.id));
  console.log("PASS a room I left does not list");

  await tab("Unread").click();
  await row(second).waitFor();
  await panel.locator(".threads-mark-all").click();
  await panel.getByText("No unread threads", { exact: true }).waitFor();
  await panel.getByText("You're all caught up", { exact: true }).waitFor();
  assert.equal((await totals()).unread_threads, "0");
  assert.equal((await totals()).unread_mentions, "0");
  assert.equal((await roomRead(beta)).unread_replies, "0");
  assert.notEqual((await roomRead(alpha)).unread_roots, "0");
  console.log("PASS mark all clears every thread, and no room's roots");

  await panel.locator(".threads-back").click();
  await page.locator(".threads-row").waitFor();
  assert.equal(new URL(page.url()).searchParams.get("threads"), null);
  await waitFor(
    async () => !(await badge.isVisible()),
    "no badge once all is read",
  );
  await page.goBack();
  await panel.waitFor();

  // The chips, over real threads of every kind.
  const chipRoom = await newRoom("inbox-chips");
  const followedRoot = async (token, room, text, reply) => {
    const root = await say(token, room, text);
    if (token !== me.token) await follow(root);
    await say(peer.token, room, reply, root.id);
    return root;
  };
  const wrote = await followedRoot(me.token, chipRoom, "Mine", "Answering you");
  const answered = await say(peer.token, chipRoom, "One I answered");
  await say(me.token, chipRoom, "My answer", answered.id);
  await say(peer.token, chipRoom, "After my answer", answered.id);
  const named = await followedRoot(
    peer.token,
    chipRoom,
    "Naming nobody",
    "@" + username + " this reply names you",
  );
  const rootNamed = await followedRoot(
    peer.token,
    chipRoom,
    "@" + username + " this root names you",
    "A plain reply",
  );
  const both = await followedRoot(
    me.token,
    chipRoom,
    "Mine again",
    "@" + username + " on your own root",
  );
  const plain = await followedRoot(
    peer.token,
    chipRoom,
    "Nothing to do with me",
    "A plain reply",
  );
  const dm = await call("/api/v1/direct-messages", me.token, {
    user_id: peer.user.id,
  });
  const direct = await followedRoot(
    peer.token,
    dm,
    "In our conversation",
    "A plain reply",
  );
  const chipThreads = [wrote, answered, named, rootNamed, both, plain, direct];
  const expectRows = async (expected) => {
    const ids = expected.map((root) => root.id);
    await waitFor(
      async () => {
        const listed = await rows();
        return chipThreads.every(
          (root) => listed.includes(root.id) === ids.includes(root.id),
        );
      },
      "the rows of " + JSON.stringify(ids),
    );
  };
  await tab("Followed").click();
  await expectRows(chipThreads);
  assert.equal(await chip("All").getAttribute("aria-pressed"), "true");
  await chip("Participated").click();
  await expectRows([wrote, answered, both, direct]);
  await chip("Mentioned").click();
  await expectRows([named, rootNamed, both]);
  await chip("Others").click();
  await expectRows([plain]);
  await chip("All").click();
  await expectRows(chipThreads);
  console.log("PASS the chips sort real threads, a direct one included");

  await chip("Mentioned").click();
  await expectRows([named, rootNamed, both]);
  const filterInAddress = () =>
    new URL(page.url()).searchParams.get("threadsFilter");
  assert.equal(filterInAddress(), "mentioned");
  await tab("Unread").click();
  await expectRows([named, rootNamed, both]);
  assert.equal(filterInAddress(), "mentioned");
  assert.equal(await chip("Mentioned").getAttribute("aria-pressed"), "true");
  await tab("Followed").click();
  await row(rootNamed).click();
  await page.locator(".thread-pane").getByText("A plain reply").waitFor();
  assert.equal(filterInAddress(), "mentioned");
  assert.equal(new URL(page.url()).searchParams.get("thread"), rootNamed.id);
  assert.equal(await chip("Mentioned").getAttribute("aria-pressed"), "true");
  await panel
    .locator('.inbox-row.selected[data-thread="' + rootNamed.id + '"]')
    .waitFor();
  await page.reload();
  await page.locator(".thread-pane").getByText("A plain reply").waitFor();
  await panel.waitFor();
  assert.equal(await chip("Mentioned").getAttribute("aria-pressed"), "true");
  await expectRows([named, rootNamed, both]);
  await page.locator(".thread-pane .thread-back").click();
  await page.locator(".thread-pane").waitFor({ state: "hidden" });
  assert.equal(filterInAddress(), "mentioned");
  await expectRows([named, rootNamed, both]);
  console.log("PASS a chip survives a tab, a thread opened and a reload");

  // Read, a reply's mention is gone from the listing: the known limit.
  await row(named).click();
  await page
    .locator(".thread-pane")
    .getByText("this reply names you")
    .waitFor();
  await expectRows([rootNamed, both]);
  await page.locator(".thread-pane .thread-back").click();
  await page.locator(".thread-pane").waitFor({ state: "hidden" });
  await chip("Others").click();
  await expectRows([plain, named]);
  await panel.locator(".threads-back").click();
  await page.locator(".threads-row").waitFor();
  assert.equal(filterInAddress(), null);
  await page.locator(".threads-row").click();
  await panel.waitFor();
  assert.equal(await chip("All").getAttribute("aria-pressed"), "true");
  await expectRows(chipThreads);
  console.log("PASS leaving the panel forgets the chip");
  await call("/api/v1/threads/read", me.token, {
    operation_id: crypto.randomUUID(),
  });

  // An account following nothing sees the Followed empty state.
  const lonely = await browser.newPage({ locale: "en-US" });
  await lonely.route("**/api/v1/threads?filter=followed*", (route) =>
    route.fulfill({ json: { threads: [], next: null } }),
  );
  await lonely.goto(base + "/?threads=followed");
  await lonely.getByLabel("Username or email").fill(username);
  await lonely.getByLabel("Password", { exact: true }).fill(password);
  await lonely.getByRole("button", { name: "Sign in", exact: true }).click();
  await lonely.getByText("You follow no thread", { exact: true }).waitFor();
  const lonelyPanel = lonely.locator(".threads-panel");
  const lonelyChip = (name) =>
    lonelyPanel.getByRole("button", { name, exact: true });
  await lonelyChip("Participated").click();
  await lonely
    .getByText("No thread you took part in", { exact: true })
    .waitFor();
  await lonelyChip("Mentioned").click();
  await lonely.getByText("No thread mentions you", { exact: true }).waitFor();
  await lonelyChip("Others").click();
  await lonely.getByText("No other thread", { exact: true }).waitFor();
  await lonelyChip("All").click();
  await lonely.getByText("You follow no thread", { exact: true }).waitFor();
  console.log("PASS each chip says why it is empty");
  await lonely.unroute("**/api/v1/threads?filter=followed*");
  // A failed load offers a retry that works.
  await lonely.route("**/api/v1/threads?filter=unread*", (route) =>
    route.fulfill({ status: 500, json: { error: "internal" } }),
  );
  await lonely
    .locator(".threads-panel")
    .getByRole("tab", { name: "Unread", exact: true })
    .click();
  const retry = lonely.locator(".threads-panel .inbox-retry");
  await retry.waitFor();
  await lonely.unroute("**/api/v1/threads?filter=unread*");
  await retry.click();
  await lonely.getByText("No unread threads", { exact: true }).waitFor();
  console.log("PASS the empty states and the retry");

  // The next page on scroll, its failure and its retry.
  const sample = (await call("/api/v1/threads?filter=followed", me.token))
    .threads[0];
  const fake = (index) => ({
    ...sample,
    root: { ...sample.root, id: "fake-" + index },
    last_reply_position: String(1000 - index),
  });
  let failNext = true;
  await lonely.route("**/api/v1/threads?filter=followed*", (route) => {
    const before = new URL(route.request().url()).searchParams.get("before");
    if (before && failNext) {
      failNext = false;
      return route.fulfill({ status: 500, json: { error: "internal" } });
    }
    return route.fulfill({
      json: before
        ? {
            threads: Array.from({ length: 5 }, (_, i) => fake(50 + i)),
            next: null,
          }
        : {
            threads: Array.from({ length: 50 }, (_, i) => fake(i)),
            next: "950",
          },
    });
  });
  await lonely
    .locator(".threads-panel")
    .getByRole("tab", { name: "Followed", exact: true })
    .click();
  const loaded = lonely.locator(".threads-panel .inbox-row");
  await loaded.nth(49).waitFor();
  const list = lonely.locator(".threads-panel .inbox-list");
  await list.evaluate((node) => {
    node.scrollTop = node.scrollHeight;
  });
  await lonely.locator(".threads-panel .inbox-list > .inbox-retry").click();
  await loaded.nth(54).waitFor();
  assert.equal(await loaded.count(), 55);
  console.log("PASS the next page loads on scroll and retries");

  // A chip matching nothing reads on, four pages per action, then asks.
  const plainEntry = (
    await call("/api/v1/threads?filter=followed&limit=100", me.token)
  ).threads.find((entry) => entry.root.id === plain.id);
  assert.ok(plainEntry, "the plain thread is listed");
  const quiet = { ...plainEntry, unread: "0", mentions: "0" };
  const farRow = (page, index) => ({
    ...quiet,
    root: { ...quiet.root, id: "far-" + page + "-" + index },
    last_reply_position: String(100000 - page * 100 - index),
  });
  let extra = 0;
  await lonely.unroute("**/api/v1/threads?filter=followed*");
  await lonely.route("**/api/v1/threads?filter=followed*", (route) => {
    const before = new URL(route.request().url()).searchParams.get("before");
    if (!before)
      return route.fulfill({
        json: {
          threads: Array.from({ length: 50 }, (_, i) => farRow(0, i)),
          next: "page-1",
        },
      });
    extra++;
    return route.fulfill({
      json:
        extra === 9
          ? {
              threads: [
                {
                  ...farRow(extra, 0),
                  root: { ...quiet.root, id: "far-match" },
                  unread: "1",
                  mentions: "1",
                },
              ],
              next: null,
            }
          : {
              threads: Array.from({ length: 50 }, (_, i) => farRow(extra, i)),
              next: "page-" + (extra + 1),
            },
    });
  });
  await lonelyPanel.getByRole("tab", { name: "Unread", exact: true }).click();
  await lonelyPanel.getByRole("tab", { name: "Followed", exact: true }).click();
  await loaded.nth(49).waitFor();
  await lonely.waitForTimeout(500);
  assert.equal(extra, 0, "All fills the view with its first page");
  await lonelyChip("Mentioned").click();
  const loadMore = lonelyPanel.locator(".inbox-more");
  await loadMore.waitFor();
  assert.equal(extra, 4, "four extra pages for the chip");
  await lonely.waitForTimeout(800);
  assert.equal(extra, 4, "and no more without asking");
  assert.equal(await loaded.count(), 0);
  await lonely.getByText("No thread mentions you", { exact: true }).waitFor();
  await lonely.getByText("Older threads may match", { exact: true }).waitFor();
  await loadMore.click();
  await waitFor(
    async () => extra === 8 && (await loadMore.isVisible()),
    "four more pages for the button",
  );
  await lonely.waitForTimeout(800);
  assert.equal(extra, 8);
  await loadMore.click();
  await lonelyPanel.locator('[data-thread="far-match"]').waitFor();
  assert.equal(extra, 9);
  assert.equal(await loaded.count(), 1);
  assert.equal(await loadMore.count(), 0, "the last page leaves no button");
  await lonelyChip("Others").click();
  await loaded.nth(49).waitFor();
  await lonely.waitForTimeout(300);
  assert.equal(extra, 9);
  await lonely.close();
  console.log("PASS a chip reads on, bounded, and nothing silently vanishes");

  assert.deepEqual(failures, [], "no page error");
} catch (error) {
  await page.screenshot({
    path: "../../.cache/web-shots/thread-inbox-failure.png",
  });
  throw error;
} finally {
  await browser.close();
}
