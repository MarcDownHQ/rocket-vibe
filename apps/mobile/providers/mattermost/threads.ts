/**
 * The account's threads. The server lists them per team, each list holding the
 * DMs and groups as well, so several teams are merged and deduplicated here.
 */

import type { ThreadInboxEntry, ThreadInboxFilter, ThreadInboxPage, ThreadParticipant, ThreadTotals } from '../../lib/provider.ts';
import { MmError, type MmClient } from './client.ts';
import type { MmDirectory } from './directory.ts';
import type { MmLive } from './live.ts';
import { record, type MmTranslator } from './translator.ts';

const PAGE = 25;
const TOTALS_PAGE = 100;

type Doc = Record<string, unknown>;

function text(doc: Doc, key: string): string | null {
  const value = doc[key];
  return typeof value === 'string' && value !== '' ? value : null;
}

function int(doc: Doc, key: string): number {
  const value = doc[key];
  return typeof value === 'number' ? value : 0;
}

function docs(value: unknown): Doc[] {
  return Array.isArray(value) ? value.map(record).filter((doc): doc is Doc => doc !== null) : [];
}

/** Each team's position, empty for its start, the teams with nothing left being absent. */
export function parseThreadCursor(cursor: string): Map<string, string> {
  const positions = new Map<string, string>();
  for (const part of cursor.split(',')) {
    const colon = part.indexOf(':');
    if (colon >= 0) positions.set(part.slice(0, colon), part.slice(colon + 1));
  }
  return positions;
}

function printCursor(positions: Map<string, string>): string | null {
  if (positions.size === 0) return null;
  return [...positions].sort(([a], [b]) => (a < b ? -1 : 1)).map(([team, before]) => `${team}:${before}`).join(',');
}

/** One team's page as the server answered it. */
export type TeamPage = { team: string; before: string | null; threads: Doc[]; full: boolean };

/**
 * Merges by last reply, newest first. A team with more to read limits what can
 * be shown: anything older than its last entry might sort after what its next
 * page brings, so it waits for that page.
 */
export function mergeTeamPages(pages: TeamPage[]): { threads: Doc[]; next: string | null } {
  let floor = Number.NEGATIVE_INFINITY;
  for (const page of pages) {
    const last = page.threads[page.threads.length - 1];
    if (page.full && last !== undefined) floor = Math.max(floor, int(last, 'last_reply_at'));
  }
  const next = new Map<string, string>();
  const seen = new Set<string>();
  const merged: Doc[] = [];
  for (const page of pages) {
    const shown = page.threads.filter((thread) => int(thread, 'last_reply_at') >= floor);
    if (page.full || shown.length < page.threads.length) {
      const last = shown[shown.length - 1];
      next.set(page.team, (last === undefined ? null : text(last, 'id')) ?? page.before ?? '');
    }
    for (const thread of shown) {
      const id = text(thread, 'id');
      if (id !== null && !seen.has(id)) {
        seen.add(id);
        merged.push(thread);
      }
    }
  }
  merged.sort((a, b) => int(b, 'last_reply_at') - int(a, 'last_reply_at'));
  return { threads: merged, next: printCursor(next) };
}

export class MmThreads {
  private readonly client: MmClient;
  private readonly directory: MmDirectory;
  private readonly live: MmLive;
  private readonly translator: MmTranslator;

  constructor(options: { client: MmClient; directory: MmDirectory; live: MmLive; translator: MmTranslator }) {
    this.client = options.client;
    this.directory = options.directory;
    this.live = options.live;
    this.translator = options.translator;
  }

  private async teams(): Promise<string[]> {
    const teams = await this.client.get<unknown>('/users/me/teams');
    return docs(teams).flatMap((team) => text(team, 'id') ?? []);
  }

  /** A page left empty by deleted roots says nothing about the next one: it reads on. */
  async page(filter: ThreadInboxFilter, cursor: string | null): Promise<ThreadInboxPage> {
    let page = await this.pageFrom(filter, cursor);
    while (page.entries.length === 0 && page.next !== null) page = await this.pageFrom(filter, page.next);
    return page;
  }

  private async pageFrom(filter: ThreadInboxFilter, cursor: string | null): Promise<ThreadInboxPage> {
    const known = cursor === null ? null : parseThreadCursor(cursor);
    const pages: TeamPage[] = [];
    for (const team of await this.teams()) {
      const position = known === null ? '' : known.get(team);
      if (position === undefined) continue;
      const before = position === '' ? null : position;
      const answer = await this.client.get<Doc>(`/users/me/teams/${team}/threads`, {
        query: { per_page: PAGE, extended: true, unread: filter === 'unread' ? true : undefined, before: before ?? undefined },
      });
      const threads = docs(answer.threads);
      pages.push({ team, before, threads, full: threads.length >= PAGE });
    }
    const { threads, next } = mergeTeamPages(pages);
    const posts = threads.map((thread) => record(thread.post)).filter((post): post is Doc => post !== null);
    await this.live.ensureAuthors(posts);
    await this.directory.ensure(threads.flatMap((thread) => docs(thread.participants).flatMap((person) => text(person, 'id') ?? [])));
    const entries: ThreadInboxEntry[] = [];
    const rooms = new Map<string, string>();
    for (const thread of threads) {
      const post = record(thread.post);
      if (post === null || int(post, 'delete_at') > 0) continue;
      const root = this.translator.toMessage(post);
      if (root === null) continue;
      let room = rooms.get(root.rid);
      if (room === undefined) {
        room = await this.roomName(root.rid);
        rooms.set(root.rid, room);
      }
      entries.push({
        root,
        room,
        lastReply: int(thread, 'last_reply_at'),
        replies: int(thread, 'reply_count'),
        participants: this.participants(thread),
        unreadReplies: int(thread, 'unread_replies'),
        unreadMentions: int(thread, 'unread_mentions'),
      });
    }
    return { entries, next };
  }

  private participants(thread: Doc): ThreadParticipant[] {
    return docs(thread.participants).flatMap((person) => {
      const id = text(person, 'id');
      const username = id === null ? null : (text(person, 'username') ?? this.directory.username(id));
      return id === null || username === null ? [] : [{ id, username }];
    });
  }

  private async roomName(rid: string): Promise<string> {
    if (!this.live.channels.has(rid)) await this.live.load(rid).catch(() => false);
    const channel = this.live.channels.get(rid);
    return channel === undefined ? '' : (this.translator.toRoom({ channel })?.displayName ?? '');
  }

  /** Counted from the unread threads themselves: each team's own totals count the DMs and groups again. */
  async totals(): Promise<ThreadTotals> {
    const seen = new Set<string>();
    let unreadMentions = 0;
    for (const team of await this.teams()) {
      const answer = await this.client.get<Doc>(`/users/me/teams/${team}/threads`, {
        query: { unread: true, threadsOnly: true, per_page: TOTALS_PAGE },
      });
      for (const thread of docs(answer.threads)) {
        const id = text(thread, 'id');
        if (id !== null && !seen.has(id)) {
          seen.add(id);
          unreadMentions += int(thread, 'unread_mentions');
        }
      }
    }
    return { unreadThreads: seen.size, unreadMentions };
  }

  /** The thread's team: its channel's, any of mine for a DM or a group. */
  private async teamOf(rid: string): Promise<string> {
    if (!this.live.channels.has(rid)) await this.live.load(rid).catch(() => false);
    const channel = this.live.channels.get(rid);
    const team = (channel === undefined ? null : text(channel, 'team_id')) ?? (await this.teams())[0];
    if (team === undefined) throw new Error('users/me/teams: no team');
    return team;
  }

  async markRead(rid: string, root: string, lastReply?: number | null): Promise<void> {
    const team = await this.teamOf(rid);
    const at = Math.max(lastReply ?? 0, Date.now());
    await this.client.put(`/users/me/teams/${team}/threads/${root}/read/${at}`);
  }

  async markAllRead(): Promise<void> {
    for (const team of await this.teams()) await this.client.put(`/users/me/teams/${team}/threads/read`);
  }

  async follow(rid: string, root: string, put: boolean): Promise<void> {
    const path = `/users/me/teams/${await this.teamOf(rid)}/threads/${root}/following`;
    if (put) await this.client.put(path);
    else await this.client.delete(path);
  }

  /** The server has no such field: a thread I do not follow answers 404. */
  async following(rid: string, root: string): Promise<boolean> {
    try {
      await this.client.get(`/users/me/teams/${await this.teamOf(rid)}/threads/${root}`);
      return true;
    } catch (e) {
      if (e instanceof MmError && e.status === 404) return false;
      throw e;
    }
  }
}
