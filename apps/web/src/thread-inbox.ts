import type { ThreadInboxEntry } from "./protocol";

export type InboxTab = "followed" | "unread";

export function inboxAddress(search: string): {
  tab?: InboxTab;
  thread?: string;
} {
  const params = new URLSearchParams(search),
    tab = params.get("threads");
  return {
    tab: tab === "followed" || tab === "unread" ? tab : undefined,
    thread: params.get("thread") || undefined,
  };
}

export function inboxUrl(
  room: string | undefined,
  tab: InboxTab | undefined,
  thread?: string,
): string {
  const params = new URLSearchParams();
  if (tab) params.set("threads", tab);
  if (thread) params.set("thread", thread);
  const query = params.toString();
  return (
    (room ? "/room/" + encodeURIComponent(room) : "/") +
    (query ? "?" + query : "")
  );
}

const newestFirst = (a: ThreadInboxEntry, b: ThreadInboxEntry) => {
  const left = BigInt(a.last_reply_position),
    right = BigInt(b.last_reply_position);
  return left === right ? 0 : left > right ? -1 : 1;
};

/** A later page: a thread that moved between two pages appears once, as last seen. */
export function appendPage(
  current: ThreadInboxEntry[],
  page: ThreadInboxEntry[],
): ThreadInboxEntry[] {
  const seen = new Set(page.map((entry) => entry.root.id));
  return [...current.filter((entry) => !seen.has(entry.root.id)), ...page].sort(
    newestFirst,
  );
}

/**
 * The first page read again: it replaces everything it covers. Rows older than
 * its last one stay only when it was not the whole list.
 */
export function refreshHead(
  current: ThreadInboxEntry[],
  head: ThreadInboxEntry[],
  headIsComplete: boolean,
): ThreadInboxEntry[] {
  if (headIsComplete) return head;
  const seen = new Set(head.map((entry) => entry.root.id)),
    oldest = head.at(-1);
  return [
    ...head,
    ...current.filter(
      (entry) =>
        !seen.has(entry.root.id) &&
        oldest &&
        BigInt(entry.last_reply_position) < BigInt(oldest.last_reply_position),
    ),
  ];
}

/** At most 100 rows are read back at once. */
export const refreshLimit = (shown: number): number =>
  Math.min(100, Math.max(50, shown));

export const countLabel = (value: string): string =>
  BigInt(value) > 99n ? "99+" : value;
