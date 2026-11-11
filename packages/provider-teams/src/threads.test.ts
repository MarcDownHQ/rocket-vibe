import assert from 'node:assert/strict';
import { test } from 'node:test';

import type { Store } from '@rocket-vibe/provider-api/sync';
import { TeamsRooms, TeamsThreads } from './sync.ts';
import { TeamsDirectory } from './translate.ts';
import { TeamsThreadInbox, TeamsThreadMarks, feedItems, groupThreads, threadState, type TeamsThreadSource } from './threads.ts';

type Doc = Record<string, unknown>;

const ME = '8:orgid:me';
const CHANNEL = '19:alpha@thread.tacv2';
const OTHER = '19:beta@thread.tacv2';
const CHAT = '19:gamma@thread.v2';
const ADA = '8:orgid:ada';
const BOB = '8:orgid:bob';

function item(activity: Doc, isread: unknown = 'true'): Doc {
  return {
    id: String(Math.random()).slice(2),
    messagetype: 'Text',
    properties: { isread, activity: { sourceThreadType: 'topic', targetUserId: ME, count: 1, ...activity } },
  };
}

function reply(at: string, extra: Doc = {}, isread: unknown = 'true'): Doc {
  return item({
    activityType: 'reply', activitySubtype: '', activityTimestamp: at, sourceThreadId: CHANNEL, sourceReplyChainId: '1000',
    sourceMessageId: String(Date.parse(at)), sourceUserId: ADA, sourceUserImDisplayName: 'Ada', messagePreview: 'a reply',
    sourceThreadTopic: 'General', ...extra,
  }, isread);
}

const FEED = {
  messages: [
    reply('2026-11-05T10:00:00.000Z', { activityType: 'replyToReply', sourceUserId: BOB, sourceUserImDisplayName: 'Bob' }, false),
    item({
      activityType: 'mention', activitySubtype: 'person', activityTimestamp: '2026-11-04T09:00:00.000Z', sourceThreadId: OTHER,
      sourceReplyChainId: 2000, sourceMessageId: 2100, sourceUserId: BOB, sourceUserImDisplayName: 'Bob', messagePreview: 'hey @Me',
    }, true),
    reply('2026-11-03T08:00:00.000Z'),
    item({
      activityType: 'reaction', activitySubtype: 'like', activityTimestamp: '2026-11-06T12:00:00.000Z', sourceThreadId: OTHER,
      sourceReplyChainId: '2000', sourceMessageId: '2000', sourceUserId: ADA,
    }),
    item({
      activityType: 'reaction', activitySubtype: 'like', activityTimestamp: '2026-11-02T12:00:00.000Z', sourceThreadId: OTHER,
      sourceReplyChainId: '2000', sourceMessageId: '2150', sourceUserId: ADA,
    }),
    item({
      activityType: 'mentionInChat', activitySubtype: 'person', activityTimestamp: '2026-11-06T13:00:00.000Z', sourceThreadId: CHAT,
      sourceThreadType: 'chat', sourceReplyChainId: '3000', sourceMessageId: '3000', sourceUserId: ADA,
    }),
    item({
      activityType: 'reply', activityTimestamp: '2026-11-06T14:00:00.000Z', sourceThreadId: CHAT, sourceThreadType: 'chat',
      sourceReplyChainId: '3100', sourceMessageId: '3100', sourceUserId: ADA,
    }),
    item({ activityType: 'call', activitySubtype: 'missedCall', activityTimestamp: '2026-11-06T15:00:00.000Z', sourceThreadId: CHAT }),
  ],
};

const AT = (iso: string) => Date.parse(iso);

test('the feed keeps channel thread items only, ids and read flags of either type', () => {
  const items = feedItems(FEED);
  assert.deepEqual(items.map((i) => [i.kind, i.rid, i.root, i.message]), [
    ['replyToReply', CHANNEL, '1000', String(AT('2026-11-05T10:00:00.000Z'))],
    ['mention', OTHER, '2000', '2100'],
    ['reply', CHANNEL, '1000', String(AT('2026-11-03T08:00:00.000Z'))],
    ['reaction', OTHER, '2000', '2150'],
  ]);
  assert.deepEqual(items.map((i) => i.isRead), [false, true, true, true]);
  assert.deepEqual(feedItems({}), []);
  assert.deepEqual(feedItems({ messages: [null, 3, { properties: { activity: { activityType: 'reply' } } }] }), []);
});

test('items group by reply chain, latest activity first, with who took part', () => {
  const [first, second] = groupThreads(feedItems(FEED));
  assert.equal(first!.rootId, `${CHANNEL}/1000`);
  assert.equal(first!.lastReply, AT('2026-11-05T10:00:00.000Z'));
  assert.deepEqual(first!.participants, [{ id: BOB, name: 'Bob' }, { id: ADA, name: 'Ada' }]);
  assert.equal(first!.participated, true);
  assert.equal(first!.mentions, 0);
  assert.equal(first!.replyIds.length, 2);
  assert.equal(second!.rootId, `${OTHER}/2000`);
  assert.equal(second!.lastReply, AT('2026-11-04T09:00:00.000Z'));
  assert.equal(second!.participated, false);
  assert.equal(second!.mentions, 1);
  assert.deepEqual(second!.participants, [{ id: BOB, name: 'Bob' }]);
});

test('an item the feed repeats across pages counts once', () => {
  const twice = feedItems({ messages: [...FEED.messages, FEED.messages[0]] });
  assert.equal(groupThreads(twice)[0]!.replyIds.length, 2);
});

test('unread is the mark against the last reply, or the feed read flag without a mark', () => {
  const [first, second] = groupThreads(feedItems(FEED));
  assert.deepEqual(threadState(first!, null), { unread: true, replies: 1, mentions: 0 });
  assert.deepEqual(threadState(second!, null), { unread: false, replies: 0, mentions: 0 });
  assert.deepEqual(threadState(first!, first!.lastReply), { unread: false, replies: 0, mentions: 0 });
  assert.deepEqual(threadState(first!, AT('2026-11-01T00:00:00.000Z')), { unread: true, replies: 2, mentions: 0 });
  assert.deepEqual(threadState(second!, AT('2026-11-01T00:00:00.000Z')), { unread: true, replies: 1, mentions: 1 });
});

function memoryStore(): Store & { cursors: Map<string, number> } {
  const cursors = new Map<string, number>();
  return {
    cursors,
    readCursor: async (scope: string, stream: string) => cursors.get(`${scope}|${stream}`) ?? null,
    writeCursor: async (scope: string, stream: string, value: number) => {
      const key = `${scope}|${stream}`;
      if ((cursors.get(key) ?? -Infinity) < value) cursors.set(key, value);
    },
  } as unknown as Store & { cursors: Map<string, number> };
}

function inbox(source: Partial<TeamsThreadSource> = {}, store: Store | null = memoryStore()) {
  const rooms = new TeamsRooms();
  rooms.entries.set(CHANNEL, { entry: { id: CHANNEL, displayName: 'General' }, team: { id: 'team', name: 'Design' } });
  const fetched: string[] = [];
  const value = new TeamsThreadInbox({
    source: {
      feed: async () => [FEED],
      fetch: async (id) => {
        fetched.push(id);
        return id === `${CHANNEL}/1000`
          ? { id: '1000', messagetype: 'RichText/Html', content: '<p>the root</p>', from: `https://chat.example/v1/users/ME/contacts/${ME}`, imdisplayname: 'Me', originalarrivaltime: '2026-11-01T00:00:00.000Z' }
          : null;
      },
      ...source,
    },
    rooms,
    threads: new TeamsThreads(),
    directory: new TeamsDirectory(),
    me: ME,
    marks: new TeamsThreadMarks(() => store),
  });
  return { value, fetched };
}

test('the followed tab lists every thread, its root read from Teams or previewed from the feed', async () => {
  const { value, fetched } = inbox();
  const page = await value.page('followed', null);
  assert.equal(page.next, null);
  assert.deepEqual(page.entries.map((e) => e.root.id), [`${CHANNEL}/1000`, `${OTHER}/2000`]);
  const [first, second] = page.entries;
  assert.equal(first!.root.text, 'the root');
  assert.equal(first!.root.authorId, ME);
  assert.equal(first!.root.rid, CHANNEL);
  assert.equal(first!.room, 'Design · General');
  assert.equal(first!.participated, true);
  assert.equal(first!.mentioned, false);
  assert.equal(first!.unreadReplies, 1);
  assert.deepEqual(first!.participants.map((p) => p.username), ['Bob', 'Ada']);
  assert.equal(second!.root.text, 'hey @Me');
  assert.equal(second!.root.rid, OTHER);
  assert.equal(second!.mentioned, true);
  assert.equal(second!.participated, false);
  assert.equal(second!.unreadReplies, 0);
  await value.page('followed', null);
  assert.deepEqual(fetched, [`${CHANNEL}/1000`, `${OTHER}/2000`, `${OTHER}/2000`]);
});

test('reading a thread marks it here only, and the unread tab and totals follow', async () => {
  const store = memoryStore();
  const { value } = inbox({}, store);
  let told = 0;
  const release = value.onChanged(() => { told += 1; });
  assert.deepEqual(await value.totals(), { unreadThreads: 1, unreadMentions: 0 });
  assert.deepEqual((await value.page('unread', null)).entries.map((e) => e.root.id), [`${CHANNEL}/1000`]);

  await value.markRead(CHANNEL, `${CHANNEL}/1000`, AT('2026-11-03T08:00:00.000Z'));
  assert.equal(told, 1);
  assert.equal(store.cursors.get(`${CHANNEL}|teams-thread-seen/1000`), AT('2026-11-05T10:00:00.000Z'));
  assert.deepEqual(await value.totals(), { unreadThreads: 0, unreadMentions: 0 });
  assert.deepEqual((await value.page('unread', null)).entries, []);
  release();
});

test('mark all reads every listed thread, a later reply unreads it again', async () => {
  let feed: Doc[] = [FEED];
  const { value } = inbox({ feed: async () => feed });
  await value.markAllRead();
  assert.deepEqual(await value.totals(), { unreadThreads: 0, unreadMentions: 0 });
  feed = [{ messages: [item({
    activityType: 'mention', activitySubtype: 'person', activityTimestamp: '2026-11-07T09:00:00.000Z', sourceThreadId: OTHER,
    sourceReplyChainId: '2000', sourceMessageId: '2200', sourceUserId: ADA,
  }), ...FEED.messages] }];
  assert.deepEqual(await value.totals(), { unreadThreads: 1, unreadMentions: 1 });
});

test('without a store the marks hold for the session', async () => {
  const { value } = inbox({}, null);
  await value.markRead(CHANNEL, `${CHANNEL}/1000`, null);
  assert.equal((await value.totals()).unreadThreads, 0);
});

test('a long list pages by offset', async () => {
  const many = { messages: Array.from({ length: 60 }, (_, n) => reply(new Date(AT('2026-11-01T00:00:00.000Z') + n * 60_000).toISOString(), { sourceReplyChainId: String(5000 + n) })) };
  const { value } = inbox({ feed: async () => [many], fetch: async () => null });
  const first = await value.page('followed', null);
  assert.equal(first.entries.length, 50);
  assert.equal(first.next, '50');
  const second = await value.page('followed', first.next);
  assert.equal(second.entries.length, 10);
  assert.equal(second.next, null);
});

test('a reaction moves the thread up but leaves it read', () => {
  const reaction = item({
    activityType: 'reaction', activitySubtype: 'like', activityTimestamp: '2026-11-08T12:00:00.000Z', sourceThreadId: CHANNEL,
    sourceReplyChainId: '1000', sourceMessageId: String(AT('2026-11-03T08:00:00.000Z')), sourceUserId: BOB,
  }, false);
  const [group] = groupThreads(feedItems({ messages: [reaction, reply('2026-11-03T08:00:00.000Z')] }));
  assert.equal(group!.lastReply, AT('2026-11-08T12:00:00.000Z'));
  assert.deepEqual(threadState(group!, AT('2026-11-03T08:00:00.000Z')), { unread: false, replies: 0, mentions: 0 });
  assert.deepEqual(threadState(group!, null), { unread: false, replies: 0, mentions: 0 });
});

test('only a mention of me as a person counts', () => {
  const mention = (subtype: string) => item({
    activityType: 'mention', activitySubtype: subtype, activityTimestamp: '2026-11-08T12:00:00.000Z', sourceThreadId: OTHER,
    sourceReplyChainId: '2000', sourceMessageId: '2300', sourceUserId: ADA,
  }, false);
  assert.deepEqual(feedItems({ messages: [mention('team'), mention('channel'), mention('tag')] }), []);
  const [group] = groupThreads(feedItems({ messages: [mention('person')] }));
  assert.deepEqual(threadState(group!, null), { unread: true, replies: 1, mentions: 1 });
});

