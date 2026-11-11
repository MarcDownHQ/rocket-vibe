/**
 * The account's channel threads, from Teams' activity feed (the
 * `48:notifications` conversation): its items grouped by reply chain. Teams'
 * own per-thread state is not read or written; whether a thread is unread is
 * judged here against a local mark of when it was last seen.
 */

import { threadMentioned, threadParticipated } from '@rocket-vibe/provider-api/threadScope';
import type { LocalMessage } from '@rocket-vibe/provider-api/rows';
import type { ThreadInboxEntry, ThreadInboxFilter, ThreadInboxPage, ThreadParticipant, ThreadTotals } from '@rocket-vibe/provider-api/provider';
import type { Store } from '@rocket-vibe/provider-api/sync';

import { channelRoom, messageId, mriOf, splitId, toMessage, type TeamsDirectory } from './translate.ts';
import type { TeamsRooms, TeamsThreads } from './sync.ts';

type Doc = Record<string, unknown>;

const PAGE = 50;
const ROOT_BATCH = 6;
const SEEN_STREAM = 'teams-thread-seen/';
const KINDS = new Set(['reply', 'replyToReply', 'mention', 'reaction']);

export type FeedKind = 'reply' | 'replyToReply' | 'mention' | 'reaction';

export type FeedItem = {
  kind: FeedKind;
  rid: string;
  root: string;
  message: string;
  at: number;
  user: string;
  name: string | null;
  preview: string | null;
  topic: string | null;
  count: number;
  isRead: boolean | null;
};

export type ThreadGroup = {
  rid: string;
  root: string;
  rootId: string;
  lastReply: number;
  items: FeedItem[];
  participants: { id: string; name: string | null }[];
  participated: boolean;
  mentions: number;
  replyIds: string[];
  topic: string | null;
};

export type ThreadState = { unread: boolean; replies: number; mentions: number };

function doc(value: unknown): Doc | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? (value as Doc) : null;
}

function id(value: unknown): string | null {
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  return typeof value === 'string' && value !== '' ? value : null;
}

function text(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null;
}

function instant(value: unknown): number | null {
  if (typeof value === 'number') return Number.isFinite(value) ? value : null;
  if (typeof value !== 'string' || value === '') return null;
  const at = /^\d+$/.test(value) ? Number(value) : Date.parse(value);
  return Number.isFinite(at) ? at : null;
}

function readFlag(value: unknown): boolean | null {
  if (typeof value === 'boolean') return value;
  if (value === 'true') return true;
  if (value === 'false') return false;
  return null;
}

/** The feed's channel-thread items, in the order the feed gives them. */
export function feedItems(body: unknown): FeedItem[] {
  const messages = doc(body)?.messages;
  if (!Array.isArray(messages)) return [];
  return messages.flatMap((raw): FeedItem[] => {
    const properties = doc(doc(raw)?.properties);
    const activity = doc(properties?.activity);
    const kind = text(activity?.activityType);
    if (!activity || !kind || !KINDS.has(kind)) return [];
    if (kind === 'mention' && text(activity.activitySubtype) !== 'person') return [];
    const rid = id(activity.sourceThreadId);
    const root = id(activity.sourceReplyChainId);
    const message = id(activity.sourceMessageId);
    const at = instant(activity.activityTimestamp);
    if (!rid || !root || !message || at === null || root === message) return [];
    if (!rid.endsWith('@thread.tacv2') && text(activity.sourceThreadType) !== 'topic') return [];
    const count = typeof activity.count === 'number' ? activity.count : Number(activity.count);
    return [{
      kind: kind as FeedKind,
      rid,
      root,
      message,
      at,
      user: mriOf(id(activity.sourceUserId) ?? ''),
      name: text(activity.sourceUserImDisplayName),
      preview: text(activity.messagePreview),
      topic: text(activity.sourceThreadTopic),
      count: Number.isFinite(count) && count > 0 ? count : 1,
      isRead: readFlag(properties?.isread),
    }];
  });
}

/** One group per reply chain, latest activity first. */
export function groupThreads(items: readonly FeedItem[]): ThreadGroup[] {
  const groups = new Map<string, ThreadGroup>();
  const distinct = new Set<string>();
  for (const item of items) {
    const key = `${item.kind}|${item.rid}|${item.message}|${item.at}`;
    if (distinct.has(key)) continue;
    distinct.add(key);
    const rootId = messageId(item.rid, item.root);
    const group = groups.get(rootId) ?? {
      rid: item.rid, root: item.root, rootId, lastReply: 0, items: [], participants: [], participated: false, mentions: 0, replyIds: [], topic: null,
    };
    groups.set(rootId, group);
    group.items.push(item);
    group.lastReply = Math.max(group.lastReply, item.at);
    group.topic ??= item.topic;
    if (item.kind === 'reply' || item.kind === 'replyToReply') group.participated = true;
    if (item.kind === 'mention') group.mentions += 1;
    if (item.kind !== 'reaction' && !group.replyIds.includes(item.message)) group.replyIds.push(item.message);
  }
  for (const group of groups.values()) {
    group.items.sort((a, b) => b.at - a.at);
    const seen = new Set<string>();
    group.participants = group.items.flatMap((item) => {
      if (item.kind === 'reaction' || item.user === '' || seen.has(item.user)) return [];
      seen.add(item.user);
      return [{ id: item.user, name: item.name }];
    });
  }
  return [...groups.values()].sort((a, b) => b.lastReply - a.lastReply);
}

/** Unread when a reply or mention is past `mark` (ms); with no mark, when the feed holds an unread one. */
export function threadState(group: ThreadGroup, mark: number | null): ThreadState {
  const unseen = group.items.filter((item) => item.kind !== 'reaction' && (mark === null ? item.isRead === false : item.at > mark));
  if (unseen.length === 0) return { unread: false, replies: 0, mentions: 0 };
  const replies = unseen.reduce((n, item) => n + item.count, 0);
  return { unread: true, replies: Math.max(replies, 1), mentions: unseen.filter((item) => item.kind === 'mention').length };
}

/** When each thread was last seen here, by its root: the store's cursors, scoped to the channel. */
export class TeamsThreadMarks {
  private readonly memory = new Map<string, number>();
  private readonly store: () => Store | null;

  constructor(store: () => Store | null = () => null) {
    this.store = store;
  }

  async read(rid: string, root: string): Promise<number | null> {
    const held = this.memory.get(messageId(rid, root)) ?? null;
    const stored = await this.store()?.readCursor(rid, `${SEEN_STREAM}${root}`).catch(() => null) ?? null;
    if (held === null) return stored;
    return stored === null ? held : Math.max(held, stored);
  }

  async write(rid: string, root: string, at: number): Promise<void> {
    const key = messageId(rid, root);
    this.memory.set(key, Math.max(this.memory.get(key) ?? 0, at));
    await this.store()?.writeCursor(rid, `${SEEN_STREAM}${root}`, at).catch(() => {});
  }
}

export type TeamsThreadSource = {
  feed(): Promise<Doc[]>;
  fetch(id: string): Promise<Doc | null>;
};

type Judged = { group: ThreadGroup; state: ThreadState };

export class TeamsThreadInbox {
  private readonly source: TeamsThreadSource;
  private readonly rooms: TeamsRooms;
  private readonly threads: TeamsThreads;
  private readonly directory: TeamsDirectory;
  private readonly me: string;
  private readonly marks: TeamsThreadMarks;
  private readonly roots = new Map<string, LocalMessage>();
  private readonly listeners = new Set<() => void>();
  private known = new Map<string, number>();

  constructor(options: { source: TeamsThreadSource; rooms: TeamsRooms; threads: TeamsThreads; directory: TeamsDirectory; me: string; marks: TeamsThreadMarks }) {
    this.source = options.source;
    this.rooms = options.rooms;
    this.threads = options.threads;
    this.directory = options.directory;
    this.me = options.me;
    this.marks = options.marks;
  }

  onChanged(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  changed(): void {
    for (const listener of [...this.listeners]) listener();
  }

  private async judged(): Promise<Judged[]> {
    const groups = groupThreads((await this.source.feed()).flatMap((page) => feedItems(page)));
    this.known = new Map(groups.map((group) => [group.rootId, group.lastReply]));
    return Promise.all(groups.map(async (group) => ({ group, state: threadState(group, await this.marks.read(group.rid, group.root)) })));
  }

  async page(filter: ThreadInboxFilter, cursor: string | null): Promise<ThreadInboxPage> {
    const all = await this.judged();
    const listed = filter === 'unread' ? all.filter((entry) => entry.state.unread) : all;
    const offset = cursor !== null && /^\d+$/.test(cursor) ? Number(cursor) : 0;
    const slice = listed.slice(offset, offset + PAGE);
    await this.loadRoots(slice.map((entry) => entry.group));
    return {
      entries: slice.map((entry) => this.entry(entry)),
      next: offset + PAGE < listed.length ? String(offset + PAGE) : null,
    };
  }

  async totals(): Promise<ThreadTotals> {
    const unread = (await this.judged()).filter((entry) => entry.state.unread);
    return { unreadThreads: unread.length, unreadMentions: unread.reduce((n, entry) => n + entry.state.mentions, 0) };
  }

  async markRead(rid: string, root: string, lastReply?: number | null): Promise<void> {
    const server = splitId(root)?.server ?? root;
    const at = Math.max(lastReply ?? 0, this.known.get(messageId(rid, server)) ?? 0) || Date.now();
    await this.marks.write(rid, server, at);
    this.changed();
  }

  async markAllRead(): Promise<void> {
    for (const { group } of await this.judged()) await this.marks.write(group.rid, group.root, group.lastReply);
    this.changed();
  }

  private async loadRoots(groups: ThreadGroup[]): Promise<void> {
    const missing = groups.filter((group) => !this.roots.has(group.rootId));
    for (let i = 0; i < missing.length; i += ROOT_BATCH) {
      await Promise.all(missing.slice(i, i + ROOT_BATCH).map(async (group) => {
        const raw = await this.source.fetch(group.rootId).catch(() => null);
        const root = raw ? toMessage(group.rid, raw, this.directory) : null;
        if (root) this.roots.set(group.rootId, root);
      }));
    }
  }

  private entry({ group, state }: Judged): ThreadInboxEntry {
    const fetched = this.roots.get(group.rootId) ?? null;
    const root = fetched ? this.threads.counted(fetched) : previewRoot(group);
    for (const person of group.participants) if (person.name !== null && this.directory.name(person.id) === null) this.directory.remember(person.id, person.name);
    const participants: ThreadParticipant[] = group.participants.map((person) => ({ id: person.id, username: person.name ?? this.directory.shown(person.id) }));
    return {
      root,
      room: this.roomName(group),
      lastReply: group.lastReply,
      replies: Math.max(this.threads.counters(group.rootId).count, group.replyIds.length),
      participants,
      unreadReplies: state.replies,
      unreadMentions: state.mentions,
      participated: threadParticipated(this.me, { rootAuthor: fetched?.authorId ?? '', repliers: group.participated ? [this.me] : [], direct: false }),
      mentioned: threadMentioned({ mentions: group.mentions, texts: [], names: () => false }),
    };
  }

  private roomName(group: ThreadGroup): string {
    const known = this.rooms.entries.get(group.rid);
    const room = known ? channelRoom(known.entry, known.team?.name ?? null, this.directory) : null;
    return room?.displayName ?? room?.name ?? group.topic ?? '';
  }
}

/** The root as the feed previews it, when the root itself could not be read. */
function previewRoot(group: ThreadGroup): LocalMessage {
  const oldest = [...group.items].reverse();
  const item = oldest.find((i) => i.kind !== 'reaction' && i.preview !== null) ?? oldest.find((i) => i.preview !== null) ?? oldest[0]!;
  return {
    id: group.rootId,
    rid: group.rid,
    text: item.preview,
    ts: item.at,
    authorId: item.user,
    authorName: item.name,
    systemType: null,
    threadId: null,
    threadCount: group.replyIds.length,
    threadLast: group.lastReply,
    threadShown: false,
    editedAt: null,
    md: null,
    attachments: null,
    reactions: null,
    urls: null,
    callId: null,
    encryptedRaw: null,
    pinned: false,
    starred: null,
    updatedAt: item.at,
  };
}
