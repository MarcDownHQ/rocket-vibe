/** The account's threads as the existing screens show them; the server leaves encrypted rooms out. */
import { threadMentioned, threadParticipated, writtenMention } from '@rocket-vibe/provider-api/threadScope';

import type { ThreadInboxEntry, ThreadInboxPage, ThreadTotals } from '../../lib/provider.ts';
import type { ThreadInbox, ThreadInboxTotals } from './protocol.generated.ts';
import { localMessage } from './store.ts';

export type ThreadRoom = { name: string; direct: boolean };

const count = (decimal: string): number => Number(decimal) || 0;

/** A thread that moves during paging can come back: a page holds each root once. */
export async function threadInboxPage(
  answer: ThreadInbox,
  me: { id: string; username: string },
  roomOf: (rid: string) => Promise<ThreadRoom | null>,
): Promise<ThreadInboxPage> {
  const names = writtenMention(me.username);
  const seen = new Set<string>();
  const rooms = new Map<string, ThreadRoom | null>();
  const entries: ThreadInboxEntry[] = [];
  for (const thread of answer.threads) {
    if (seen.has(thread.root.id)) continue;
    seen.add(thread.root.id);
    if (!rooms.has(thread.room_id)) rooms.set(thread.room_id, await roomOf(thread.room_id));
    const room = rooms.get(thread.room_id) ?? null;
    const root = localMessage(thread.root, me.id);
    const unreadMentions = count(thread.mentions);
    const at = Date.parse(thread.last_reply_at);
    entries.push({
      root,
      room: room?.name ?? '',
      lastReply: Number.isFinite(at) ? at : 0,
      replies: count(thread.replies),
      participants: thread.participants.map((person) => ({ id: person.id, username: person.username })),
      unreadReplies: count(thread.unread),
      unreadMentions,
      participated: threadParticipated(me.id, {
        rootAuthor: root.authorId,
        repliers: thread.participants.map((person) => person.id),
        direct: room?.direct === true,
      }),
      mentioned: threadMentioned({ mentions: unreadMentions, texts: [root.text], names }),
    });
  }
  return { entries, next: answer.next ?? null };
}

export function threadTotals(answer: ThreadInboxTotals): ThreadTotals {
  return { unreadThreads: count(answer.unread_threads), unreadMentions: count(answer.unread_mentions) };
}
