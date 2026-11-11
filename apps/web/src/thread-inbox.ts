import { mentionsMe } from "./activity-feed.ts";
import type { RoomKind, ThreadInboxEntry } from "./protocol";

export type InboxTab = "followed" | "unread";

export const INBOX_FILTERS = [
  "all",
  "participated",
  "mentioned",
  "others",
] as const;
export type InboxFilter = (typeof INBOX_FILTERS)[number];

/** Pages read beyond the visible ones for one user action. */
export const EXTRA_PAGES = 4;

export function inboxAddress(search: string): {
  tab?: InboxTab;
  thread?: string;
  filter: InboxFilter;
} {
  const params = new URLSearchParams(search),
    tab = params.get("threads"),
    known = tab === "followed" || tab === "unread",
    filter = INBOX_FILTERS.find((f) => f === params.get("threadsFilter"));
  return {
    tab: known ? tab : undefined,
    thread: params.get("thread") || undefined,
    filter: known && filter ? filter : "all",
  };
}

export function inboxUrl(
  room: string | undefined,
  tab: InboxTab | undefined,
  thread?: string,
  filter: InboxFilter = "all",
): string {
  const params = new URLSearchParams();
  if (tab) params.set("threads", tab);
  if (tab && filter !== "all") params.set("threadsFilter", filter);
  if (thread) params.set("thread", thread);
  const query = params.toString();
  return (
    (room ? "/room/" + encodeURIComponent(room) : "/") +
    (query ? "?" + query : "")
  );
}

export interface ThreadTraits {
  participated: boolean;
  mentioned: boolean;
}

export function threadTraits(
  entry: ThreadInboxEntry,
  me: { id: string; username: string },
  roomKind: RoomKind | undefined,
): ThreadTraits {
  return {
    participated:
      entry.root.author.id === me.id ||
      entry.participants.some((person) => person.id === me.id) ||
      roomKind === "direct",
    mentioned: entry.mentions !== "0" || mentionsMe(entry.root, me.username),
  };
}

export function matchesFilter(
  filter: InboxFilter,
  { participated, mentioned }: ThreadTraits,
): boolean {
  if (filter === "participated") return participated;
  if (filter === "mentioned") return mentioned;
  return filter === "all" || (!participated && !mentioned);
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
