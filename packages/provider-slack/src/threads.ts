/**
 * The account's threads, from Slack's Threads view: only followed threads,
 * latest reply first. Slack has no unread filter, so an unread page is the
 * followed list narrowed here.
 */

import { threadMentioned, threadParticipated } from '@rocket-vibe/provider-api/threadScope';
import type { ThreadInboxEntry, ThreadInboxFilter, ThreadInboxPage, ThreadParticipant, ThreadTotals } from '@rocket-vibe/provider-api/provider';

import { compareTimestamp, isTimestamp, timestampMs, type SlackClient } from './client.ts';
import type { SlackDirectory } from './directory.ts';
import type { SlackLive } from './live.ts';
import { parseMessageId, roomType, type SlackTranslator } from './translator.ts';

const PAGE = 50;
/** Followed pages one unread page may read when nothing on them is unread. */
const UNREAD_WALK = 5;

type Doc = Record<string, unknown>;

function doc(value: unknown): Doc | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? (value as Doc) : null;
}

function docs(value: unknown): Doc[] {
  return Array.isArray(value) ? value.map(doc).filter((item): item is Doc => item !== null) : [];
}

function strings(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];
}

function newer(ts: unknown, than: unknown): boolean {
  return isTimestamp(ts) && (!isTimestamp(than) || compareTimestamp(ts, than) > 0);
}

function hasUnread(root: Doc): boolean {
  return newer(root.latest_reply, root.last_read);
}

function rootOf(thread: Doc): Doc | null {
  return doc(thread.root_msg);
}

export class SlackThreads {
  private readonly client: SlackClient;
  private readonly directory: SlackDirectory;
  private readonly live: SlackLive;
  private readonly translator: SlackTranslator;
  private readonly myId: string;
  private readonly roomNames: ReadonlyMap<string, string>;

  constructor(options: { client: SlackClient; directory: SlackDirectory; live: SlackLive; translator: SlackTranslator; myId: string; roomNames?: ReadonlyMap<string, string> }) {
    this.client = options.client;
    this.directory = options.directory;
    this.live = options.live;
    this.translator = options.translator;
    this.myId = options.myId;
    this.roomNames = options.roomNames ?? new Map();
  }

  /** `current_ts` is the last thread's `latest_reply`: `max_ts` answers the same page again. */
  private async view(cursor: string | null): Promise<{ threads: Doc[]; next: string | null }> {
    const answer = await this.client.call('subscriptions.thread.getView', {
      limit: PAGE,
      priority_mode: 'all',
      ...(cursor === null ? {} : { current_ts: cursor }),
    });
    const threads = docs(answer.threads);
    const last = threads[threads.length - 1];
    const reached = last === undefined ? null : rootOf(last)?.latest_reply;
    const next = answer.has_more === true && isTimestamp(reached) && reached !== cursor ? reached : null;
    return { threads, next };
  }

  /** May answer no entries with a `next`: an unread page stops walking before the list ends. */
  async page(filter: ThreadInboxFilter, cursor: string | null): Promise<ThreadInboxPage> {
    let { threads, next } = await this.view(cursor);
    if (filter === 'unread') {
      const unread = (list: Doc[]) => list.filter((thread) => hasUnread(rootOf(thread) ?? {}));
      threads = unread(threads);
      for (let walked = 1; threads.length === 0 && next !== null && walked < UNREAD_WALK; walked++) {
        const page = await this.view(next);
        threads = unread(page.threads);
        next = page.next;
      }
    }
    return { entries: await this.entries(threads), next };
  }

  private async entries(threads: Doc[]): Promise<ThreadInboxEntry[]> {
    const people = threads.flatMap((thread) => {
      const root = rootOf(thread) ?? {};
      return [root.user, ...strings(root.reply_users)].filter((id): id is string => typeof id === 'string');
    });
    const others = threads.flatMap((thread) => {
      const channel = this.channelOf(rootOf(thread) ?? {});
      return typeof channel?.user === 'string' ? [channel.user] : [];
    });
    await this.directory.ensure([...people, ...others]);
    const mark = `<@${this.myId}>`;
    const names = (text: string) => text.includes(mark);
    return threads.flatMap((thread): ThreadInboxEntry[] => {
      const raw = rootOf(thread);
      if (raw === null) return [];
      const root = this.translator.toMessage(raw);
      if (root === null) return [];
      const replies = docs(thread.latest_replies);
      const unseen = replies.filter((reply) => newer(reply.ts, raw.last_read));
      const unread = hasUnread(raw);
      const repliers = strings(raw.reply_users);
      const texts = [raw.text, ...replies.map((reply) => reply.text)].map((text) => (typeof text === 'string' ? text : null));
      const room = this.room(root.rid);
      return [{
        root,
        room: room.name,
        lastReply: isTimestamp(raw.latest_reply) ? timestampMs(raw.latest_reply) : 0,
        replies: typeof raw.reply_count === 'number' ? raw.reply_count : 0,
        participants: this.participants(repliers),
        // Only the replies returned can be counted; the thread itself says it has one.
        unreadReplies: unread ? Math.max(unseen.length, 1) : 0,
        unreadMentions: unread ? unseen.filter((reply) => typeof reply.text === 'string' && names(reply.text)).length : 0,
        participated: threadParticipated(this.myId, { rootAuthor: root.authorId, repliers, direct: room.direct }),
        mentioned: threadMentioned({ mentions: 0, texts, names }),
      }];
    });
  }

  private channelOf(raw: Doc): Doc | null {
    return typeof raw.channel === 'string' ? (this.live.channels.get(raw.channel) ?? null) : null;
  }

  private room(rid: string): { name: string; direct: boolean } {
    const channel = this.live.channels.get(rid);
    if (channel === undefined) return { name: this.roomNames.get(rid) ?? '', direct: rid.startsWith('D') };
    const name = this.translator.toRoom({ channel })?.displayName ?? this.roomNames.get(rid) ?? '';
    return { name, direct: roomType(channel) === 'd' };
  }

  private participants(ids: string[]): ThreadParticipant[] {
    return ids.flatMap((id) => {
      const username = this.directory.username(id);
      return username === null ? [] : [{ id, username }];
    });
  }

  /** Unread threads on the view's first page; mentions from `client.counts`. */
  async totals(): Promise<ThreadTotals> {
    const [counts, view] = await Promise.all([this.client.call('client.counts', { thread_counts_by_channel: true }), this.view(null)]);
    const mentions = doc(counts.threads)?.mention_count;
    return {
      unreadThreads: view.threads.filter((thread) => hasUnread(rootOf(thread) ?? {})).length,
      unreadMentions: typeof mentions === 'number' ? mentions : 0,
    };
  }

  /** The exact `latest_reply`: the screen's `lastReply` is in ms, too coarse for a Slack timestamp. */
  async markRead(rid: string, root: string): Promise<void> {
    const threadTs = this.threadTs(root);
    const answer = await this.client.call('conversations.replies', { channel: rid, ts: threadTs, limit: 1 });
    const latest = docs(answer.messages).find((message) => message.ts === threadTs)?.latest_reply;
    if (!isTimestamp(latest)) return;
    await this.client.call('subscriptions.thread.mark', { channel: rid, thread_ts: threadTs, ts: latest });
  }

  async markAllRead(): Promise<void> {
    await this.client.call('subscriptions.thread.clearAll');
  }

  async follow(rid: string, root: string, put: boolean): Promise<void> {
    await this.client.call(put ? 'subscriptions.thread.add' : 'subscriptions.thread.remove', { channel: rid, thread_ts: this.threadTs(root) });
  }

  async following(rid: string, root: string): Promise<boolean> {
    const threadTs = this.threadTs(root);
    const answer = await this.client.call('subscriptions.thread.get', { channel: rid, thread_ts: threadTs });
    return strings(answer.subscriptions).includes(threadTs);
  }

  private threadTs(root: string): string {
    const parsed = parseMessageId(root);
    if (parsed === null) throw new Error(`Not a Slack message id: ${root}`);
    return parsed.ts;
  }
}
