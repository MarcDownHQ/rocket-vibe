import assert from 'node:assert/strict';
import { test } from 'node:test';

import type { Session } from '@rocket-vibe/provider-api/session';
import { createSlackProvider } from './index.ts';
import { SlackClient } from './client.ts';
import { SlackDirectory } from './directory.ts';
import { SlackHistory } from './history.ts';
import { SlackLive, THREAD_TRAFFIC_MS } from './live.ts';
import { SlackThreads } from './threads.ts';
import { SlackTranslator } from './translator.ts';
import { AUTH, COOKIE, TOKEN, fakeSlack, type SlackCall } from './testing.ts';

const SESSION: Session = { baseUrl: 'https://test.slack.com', authToken: TOKEN, userId: 'U000', username: 'me', kind: 'slack', siteUrl: null, slackCookie: COOKIE, slackTeamId: 'T000' };

const root = (ts: string, over: Record<string, unknown> = {}) => ({
  type: 'message',
  user: 'U001',
  ts,
  thread_ts: ts,
  text: `root ${ts}`,
  channel: 'C1',
  reply_count: 2,
  reply_users_count: 1,
  reply_users: ['U001'],
  latest_reply: '20.000300',
  subscribed: true,
  last_read: '20.000300',
  ...over,
});
const reply = (ts: string, over: Record<string, unknown> = {}) => ({
  user: 'U001',
  ts,
  text: `reply ${ts}`,
  thread_ts: '10.000100',
  channel: 'C1',
  parent_user_id: 'U001',
  ...over,
});
const view = (threads: unknown[], over: Record<string, unknown> = {}) => ({
  body: { ok: true, threads, has_more: false, max_ts: '99.000000', new_threads_count: 0, total_unread_replies: 0, ...over },
});

class StubListener {
  state = 'closed' as const;
  connect(): void {}
  close(): void {}
  checkAlive = async () => false;
  subscribe = () => () => {};
  onEvent = () => () => {};
  onLoss = () => () => {};
  armedSubscriptions = async () => {};
  reset(): void {}
}

function account(route: (call: SlackCall) => { status?: number; body?: unknown } | undefined) {
  const server = fakeSlack((call) => {
    if (call.method === 'auth.test') return { body: AUTH };
    if (call.method === 'users.info') return { body: { ok: true, user: { id: call.args.user, name: call.args.user?.toLowerCase() } } };
    return route(call);
  });
  const rest = { onTokenRejected: () => {} };
  const provider = createSlackProvider(SESSION, rest as never, () => 'local-1', { fetch: server.fetcher, listener: new StubListener() as never });
  return { actions: provider.actions, calls: server.calls };
}

test('the followed page asks all priorities and maps a thread', async () => {
  const { actions, calls } = account((call) => {
    if (call.method === 'subscriptions.thread.getView') {
      return view([{
        root_msg: root('10.000100', { last_read: '20.000100', reply_users: ['U001', 'U002'], reply_count: 5 }),
        latest_replies: [reply('20.000200'), reply('20.000300', { text: 'hey <@U000>' }), reply('20.000100', { text: 'old <@U000>' })],
      }]);
    }
    return undefined;
  });
  const page = await actions.threadInbox!('followed', null);
  const asked = calls.find((call) => call.method === 'subscriptions.thread.getView')!;
  assert.equal(asked.args.priority_mode, 'all');
  assert.equal(asked.args.current_ts, undefined);
  assert.equal(page.next, null);
  const [entry] = page.entries;
  assert.equal(entry?.root.id, 'C1:10.000100');
  assert.equal(entry?.root.rid, 'C1');
  assert.equal(entry?.room, '');
  assert.equal(entry?.replies, 5);
  assert.equal(entry?.lastReply, 20000);
  assert.deepEqual(entry?.participants.map((p) => p.id), ['U001', 'U002']);
  assert.deepEqual([entry?.unreadReplies, entry?.unreadMentions], [2, 1]);
  assert.deepEqual([entry?.participated, entry?.mentioned], [false, true]);
});

test('the next page starts from the last thread latest reply, never max_ts', async () => {
  const { actions, calls } = account((call) => {
    if (call.method !== 'subscriptions.thread.getView') return undefined;
    return call.args.current_ts === undefined
      ? view([{ root_msg: root('10.000100', { latest_reply: '40.000000' }), latest_replies: [] }, { root_msg: root('11.000100', { latest_reply: '30.000000' }), latest_replies: [] }], { has_more: true })
      : view([{ root_msg: root('12.000100', { latest_reply: '25.000000' }), latest_replies: [] }]);
  });
  const first = await actions.threadInbox!('followed', null);
  assert.equal(first.next, '30.000000');
  const second = await actions.threadInbox!('followed', first.next);
  assert.equal(calls.at(-1)?.args.current_ts, '30.000000');
  assert.deepEqual(second.entries.map((e) => e.root.id), ['C1:12.000100']);
  assert.equal(second.next, null);
});

test('a page repeating its cursor ends the list', async () => {
  const { actions } = account((call) =>
    call.method === 'subscriptions.thread.getView'
      ? view([{ root_msg: root('10.000100', { latest_reply: '30.000000' }), latest_replies: [] }], { has_more: true })
      : undefined,
  );
  assert.equal((await actions.threadInbox!('followed', '30.000000')).next, null);
});

test('the unread page keeps latest_reply newer than last_read', async () => {
  const { actions } = account((call) =>
    call.method === 'subscriptions.thread.getView'
      ? view([
          { root_msg: root('10.000100'), latest_replies: [] },
          { root_msg: root('11.000100', { last_read: '20.000200' }), latest_replies: [] },
          { root_msg: root('12.000100', { last_read: '0000000000.000000' }), latest_replies: [reply('20.000300')] },
        ])
      : undefined,
  );
  const page = await actions.threadInbox!('unread', null);
  assert.deepEqual(page.entries.map((e) => e.root.id), ['C1:11.000100', 'C1:12.000100']);
  assert.equal(page.entries[0]?.unreadReplies, 1, 'the thread says it has one even when no reply was returned');
});

test('an unread page walks a bounded number of followed pages and hands the rest back', async () => {
  let n = 0;
  const { actions, calls } = account((call) => {
    if (call.method !== 'subscriptions.thread.getView') return undefined;
    n++;
    return view([{ root_msg: root(`${n}.000100`, { latest_reply: `${100 - n}.000000`, last_read: `${100 - n}.000000` }), latest_replies: [] }], { has_more: true });
  });
  const page = await actions.threadInbox!('unread', null);
  assert.deepEqual(page.entries, []);
  assert.equal(page.next, '95.000000');
  assert.equal(calls.filter((call) => call.method === 'subscriptions.thread.getView').length, 5);
});

test('participated and mentioned on Slack', async () => {
  const { actions } = account((call) =>
    call.method === 'subscriptions.thread.getView'
      ? view([
          { root_msg: root('1.000100'), latest_replies: [] },
          { root_msg: root('2.000100', { user: 'U000' }), latest_replies: [] },
          { root_msg: root('3.000100', { reply_users: ['U000'] }), latest_replies: [] },
          { root_msg: root('4.000100', { channel: 'D1' }), latest_replies: [] },
          { root_msg: root('5.000100', { text: 'hi <@U000>' }), latest_replies: [] },
          { root_msg: root('6.000100'), latest_replies: [reply('20.000300', { text: 'read <@U000>' })] },
          { root_msg: root('7.000100', { text: 'hi <@U0001> @me' }), latest_replies: [] },
        ])
      : undefined,
  );
  const { entries } = await actions.threadInbox!('followed', null);
  assert.deepEqual(entries.map((e) => [e.participated, e.mentioned]), [
    [false, false],
    [true, false],
    [true, false],
    [true, false],
    [false, true],
    [false, true],
    [false, false],
  ]);
  assert.deepEqual(entries.map((e) => e.unreadReplies), [0, 0, 0, 0, 0, 0, 0]);
});

test('totals: unread threads from the view, mentions from client.counts', async () => {
  const { actions, calls } = account((call) => {
    if (call.method === 'client.counts') {
      return { body: { ok: true, threads: { has_unreads: true, mention_count: 3, mention_count_by_channel: { C1: 3 }, unread_count_by_channel: { C1: 9 } } } };
    }
    if (call.method === 'subscriptions.thread.getView') {
      return view([
        { root_msg: root('1.000100', { last_read: '1.000000' }), latest_replies: [] },
        { root_msg: root('2.000100', { last_read: '1.000000' }), latest_replies: [] },
        { root_msg: root('3.000100'), latest_replies: [] },
      ], { has_more: true });
    }
    return undefined;
  });
  assert.deepEqual(await actions.threadTotals!(), { unreadThreads: 2, unreadMentions: 3 });
  assert.equal(calls.find((call) => call.method === 'client.counts')?.args.thread_counts_by_channel, 'true');
  assert.equal(calls.filter((call) => call.method === 'subscriptions.thread.getView').length, 1, 'one page only');
  assert.equal(calls.find((call) => call.method === 'subscriptions.thread.getView')?.args.limit, '50');
});

test('reading a thread marks its exact latest reply and never the room', async () => {
  const changed: number[] = [];
  const { actions, calls } = account((call) => {
    if (call.method === 'subscriptions.thread.get') return { body: { ok: true, subscriptions: ['10.000100'] } };
    if (call.method === 'conversations.replies') return { body: { ok: true, messages: [root('10.000100', { latest_reply: '20.000345' })], has_more: true } };
    if (call.method === 'subscriptions.thread.mark') return { body: { ok: true } };
    return undefined;
  });
  actions.onThreadsChanged!(() => changed.push(1));
  await actions.markThreadRead!('C1', 'C1:10.000100', 20000);
  assert.deepEqual(calls.find((call) => call.method === 'subscriptions.thread.mark')?.args, { channel: 'C1', thread_ts: '10.000100', ts: '20.000345' });
  assert.equal(calls.some((call) => call.method === 'conversations.mark'), false);
  assert.equal(changed.length, 1);
});

test('a root without replies has nothing to mark', async () => {
  const { actions, calls } = account((call) => {
    if (call.method === 'subscriptions.thread.get') return { body: { ok: true, subscriptions: ['10.000100'] } };
    return call.method === 'conversations.replies' ? { body: { ok: true, messages: [{ ts: '10.000100', channel: 'C1' }] } } : undefined;
  });
  await actions.markThreadRead!('C1', 'C1:10.000100');
  assert.equal(calls.some((call) => call.method === 'subscriptions.thread.mark'), false);
});

test('a thread I do not follow is never marked', async () => {
  const { actions, calls } = account((call) => {
    if (call.method === 'subscriptions.thread.get') return { body: { ok: true, subscriptions: [] } };
    if (call.method === 'conversations.replies') return { body: { ok: true, messages: [root('10.000100', { latest_reply: '20.000345' })] } };
    return undefined;
  });
  let changed = 0;
  actions.onThreadsChanged!(() => changed++);
  await actions.markThreadRead!('C1', 'C1:10.000100');
  assert.equal(calls.some((call) => call.method === 'subscriptions.thread.mark'), false);
  assert.equal(changed, 0);
});

test('a followed thread is marked once per latest reply', async () => {
  let latest = '20.000345';
  const { actions, calls } = account((call) => {
    if (call.method === 'subscriptions.thread.getView') return view([{ root_msg: root('10.000100', { latest_reply: latest }), latest_replies: [] }]);
    if (call.method === 'client.counts') return { body: { ok: true, threads: { mention_count: 0 } } };
    if (call.method === 'conversations.replies') return { body: { ok: true, messages: [root('10.000100', { latest_reply: latest })] } };
    if (call.method === 'subscriptions.thread.mark') return { body: { ok: true } };
    return undefined;
  });
  await actions.threadInbox!('followed', null);
  await actions.markThreadRead!('C1', 'C1:10.000100');
  await actions.markThreadRead!('C1', 'C1:10.000100');
  latest = '21.000100';
  await actions.markThreadRead!('C1', 'C1:10.000100');
  assert.deepEqual(calls.filter((call) => call.method === 'subscriptions.thread.mark').map((call) => call.args.ts), ['20.000345', '21.000100']);
  assert.equal(calls.some((call) => call.method === 'subscriptions.thread.get'), false);
});

test('mark all clears the account', async () => {
  const { actions, calls } = account((call) =>
    call.method === 'subscriptions.thread.clearAll' ? { body: { ok: true, marked: 4, undo_ts: '30.000000' } } : undefined,
  );
  let changed = 0;
  actions.onThreadsChanged!(() => changed++);
  await actions.markAllThreadsRead!();
  assert.equal(calls.filter((call) => call.method === 'subscriptions.thread.clearAll').length, 1);
  assert.equal(changed, 1);
});

test('follow, unfollow and the following answer', async () => {
  let followed = false;
  const { actions, calls } = account((call) => {
    if (call.method === 'subscriptions.thread.add') {
      followed = true;
      return { body: { ok: true } };
    }
    if (call.method === 'subscriptions.thread.remove') {
      followed = false;
      return { body: { ok: true } };
    }
    if (call.method === 'subscriptions.thread.get') return { body: { ok: true, subscriptions: followed ? ['10.000100'] : [] } };
    return undefined;
  });
  assert.equal(await actions.threadFollowing!('C1', 'C1:10.000100'), false);
  await actions.followThread!('C1', 'C1:10.000100', true);
  assert.deepEqual(calls.find((call) => call.method === 'subscriptions.thread.add')?.args, { channel: 'C1', thread_ts: '10.000100' });
  assert.equal(await actions.threadFollowing!('C1', 'C1:10.000100'), true);
  await actions.followThread!('C1', 'C1:10.000100', false);
  assert.deepEqual(calls.find((call) => call.method === 'subscriptions.thread.remove')?.args, { channel: 'C1', thread_ts: '10.000100' });
  assert.equal(await actions.threadFollowing!('C1', 'C1:10.000100'), false);
});

test('a refused view rejects instead of showing an empty list', async () => {
  const { actions } = account((call) => (call.method === 'subscriptions.thread.getView' ? { body: { ok: false, error: 'not_allowed' } } : undefined));
  await assert.rejects(actions.threadInbox!('followed', null));
});

function liveOnly() {
  const { fetcher } = fakeSlack((call) => (call.method === 'auth.test' ? { body: AUTH } : undefined));
  const client = new SlackClient(TOKEN, COOKIE, { fetch: fetcher });
  const directory = new SlackDirectory(client, 'https://test.slack.com');
  const value = new SlackLive({ client, directory, history: new SlackHistory(client, directory), myId: 'U000' });
  value.remember({ id: 'C1', name: 'general', is_channel: true }, { id: 'C1' });
  return value;
}

test('a message with a thread_ts tells the listeners, paced; any other message does not', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const value = liveOnly();
  let changed = 0;
  const release = value.onThreadsChanged(() => changed++);
  await value.expand({ type: 'message', channel: 'C1', ts: '5.000000', user: 'U001', text: 'plain' });
  t.mock.timers.tick(THREAD_TRAFFIC_MS);
  assert.equal(changed, 0);
  await value.expand({ type: 'message', channel: 'C1', ts: '6.000000', thread_ts: '1.000000', user: 'U001', text: 'a' });
  await value.expand({ type: 'message', channel: 'C1', ts: '7.000000', thread_ts: '1.000000', user: 'U001', text: 'b' });
  await value.expand({ type: 'message', subtype: 'message_replied', hidden: true, channel: 'C1', message: { ts: '1.000000', thread_ts: '1.000000' } });
  assert.equal(changed, 0);
  t.mock.timers.tick(THREAD_TRAFFIC_MS);
  assert.equal(changed, 1);
  await value.expand({ type: 'message', channel: 'C1', ts: '8.000000', thread_ts: '1.000000', user: 'U001', text: 'c' });
  release();
  t.mock.timers.tick(THREAD_TRAFFIC_MS);
  assert.equal(changed, 1, 'released, the pending call is dropped');
});

test('a listed channel and a direct conversation are named from the store, a group DM is direct', async () => {
  const { fetcher } = fakeSlack((call) => {
    if (call.method === 'auth.test') return { body: AUTH };
    if (call.method === 'users.info') return { body: { ok: true, user: { id: call.args.user, name: 'bob' } } };
    if (call.method === 'subscriptions.thread.getView') {
      return view([
        { root_msg: root('1.000100'), latest_replies: [] },
        { root_msg: root('2.000100', { channel: 'D1' }), latest_replies: [] },
        { root_msg: root('3.000100', { channel: 'G1' }), latest_replies: [] },
        { root_msg: root('4.000100', { channel: 'C9' }), latest_replies: [] },
      ]);
    }
    return undefined;
  });
  const client = new SlackClient(TOKEN, COOKIE, { fetch: fetcher });
  const directory = new SlackDirectory(client, 'https://test.slack.com');
  const live = new SlackLive({ client, directory, history: new SlackHistory(client, directory), myId: 'U000' });
  live.remember({ id: 'C1', name: 'general', is_channel: true }, null);
  live.remember({ id: 'D1', is_im: true, user: 'U005' }, null);
  live.remember({ id: 'G1', name: 'mpdm-a--b-1', is_mpim: true }, null);
  const threads = new SlackThreads({ client, directory, live, translator: new SlackTranslator(directory, 'U000'), myId: 'U000' });
  const { entries } = await threads.page('followed', null);
  assert.equal(entries[0]?.room, 'general');
  assert.equal(entries[0]?.participated, false);
  assert.notEqual(entries[1]?.room, '');
  assert.equal(entries[1]?.participated, true);
  assert.equal(entries[2]?.participated, true);
  assert.equal(entries[3]?.room, '');
  assert.equal(entries[3]?.participated, false);
});
