import assert from 'node:assert/strict';
import { test } from 'node:test';

import type { Session } from '../../lib/auth.ts';
import { RestClient } from '../../lib/rest.ts';
import { createMattermostProvider } from './index.ts';
import { MmClient } from './client.ts';
import { MmDirectory } from './directory.ts';
import { MmLive } from './live.ts';
import { fakeServer, post, type Call } from './testing.ts';
import { mergeTeamPages, parseThreadCursor, type TeamPage } from './threads.ts';

const thread = (id: string, at: number) => ({ id, last_reply_at: at });
const team = (name: string, threads: Record<string, unknown>[], full: boolean): TeamPage => ({ team: name, before: null, threads, full });
const ids = (threads: Record<string, unknown>[]) => threads.map((t) => t.id);

test('a team with more to read holds back what could sort after it', () => {
  const { threads, next } = mergeTeamPages([
    team('a', [thread('a1', 90), thread('a2', 50)], true),
    team('b', [thread('b1', 80), thread('b2', 40), thread('b3', 30)], false),
  ]);
  assert.deepEqual(ids(threads), ['a1', 'b1', 'a2']);
  const cursor = parseThreadCursor(next!);
  assert.equal(cursor.get('a'), 'a2');
  assert.equal(cursor.get('b'), 'b1');
});

test('a conversation listed by every team shows once', () => {
  const { threads, next } = mergeTeamPages([
    team('a', [thread('dm', 90), thread('a1', 50)], false),
    team('b', [thread('dm', 90)], false),
  ]);
  assert.deepEqual(ids(threads), ['dm', 'a1']);
  assert.equal(next, null);
});

test('a team read to its end leaves the cursor', () => {
  const { next } = mergeTeamPages([team('a', [thread('a1', 90)], false), team('b', [thread('b1', 80)], true)]);
  assert.deepEqual([...parseThreadCursor(next!).keys()], ['b']);
});

test('a team held back from its start is read again from its start', () => {
  const { threads, next } = mergeTeamPages([team('a', [thread('a1', 90)], true), team('b', [thread('b1', 10)], false)]);
  assert.deepEqual(ids(threads), ['a1']);
  assert.equal(parseThreadCursor(next!).get('b'), '');
});

test('a team with nothing shown keeps its position', () => {
  const { threads, next } = mergeTeamPages([
    team('a', [thread('a1', 90)], true),
    { ...team('b', [thread('b1', 10)], false), before: 'b0' },
  ]);
  assert.deepEqual(ids(threads), ['a1']);
  assert.equal(parseThreadCursor(next!).get('b'), 'b0');
});

const session = (base: string): Session => ({ kind: 'mattermost', baseUrl: base, userId: 'me', username: 'me', authToken: 'token', siteUrl: null });

const entry = (id: string, at: number, over: Record<string, unknown> = {}) => ({
  id,
  reply_count: 2,
  last_reply_at: at,
  unread_replies: 0,
  unread_mentions: 0,
  participants: [],
  post: post(id),
  ...over,
});

function account(route: (call: Call) => { status?: number; body?: unknown } | undefined) {
  const server = fakeServer((call) => {
    if (call.path === '/users/me/teams') return { body: [{ id: 't1' }, { id: 't2' }] };
    if (call.path === '/channels/ch1') return { body: { id: 'ch1', type: 'O', name: 'dev', display_name: 'Dev', team_id: 't2' } };
    if (call.path === '/channels/ch1/members/me') return { body: { channel_id: 'ch1', user_id: 'me' } };
    if (call.path === '/users/ids') return { body: [{ id: 'u-bob', username: 'bob' }, { id: 'u-eve', username: 'eve' }] };
    return route(call);
  });
  const provider = createMattermostProvider(session(server.base), new RestClient(server.base), () => 'id', {
    fetch: server.fetcher,
    listener: {} as never,
  });
  return { server, actions: provider.actions };
}

test('a page merges the teams, names the room and its participants', async () => {
  const { actions, server } = account((call) => {
    if (call.path === '/users/me/teams/t1/threads') {
      return {
        body: {
          threads: [
            entry('dm', 90, { unread_replies: 3, unread_mentions: 1, participants: [{ id: 'u-bob', username: 'bob' }, { id: 'u-eve' }, { id: 'u-gone' }] }),
            entry('r1', 50, { post: JSON.stringify(post('r1')) }),
          ],
        },
      };
    }
    if (call.path === '/users/me/teams/t2/threads') return { body: { threads: [entry('dm', 90)] } };
    return undefined;
  });
  const page = await actions.threadInbox!('unread', null);
  assert.deepEqual(page.entries.map((e) => e.root.id), ['dm', 'r1']);
  assert.equal(page.next, null);
  const [first] = page.entries;
  assert.equal(first?.room, 'Dev');
  assert.deepEqual([first?.lastReply, first?.replies, first?.unreadReplies, first?.unreadMentions], [90, 2, 3, 1]);
  assert.deepEqual(first?.participants, [{ id: 'u-bob', username: 'bob' }, { id: 'u-eve', username: 'eve' }]);
  const listed = server.calls.find((call) => call.path === '/users/me/teams/t1/threads');
  assert.equal(listed?.query.get('unread'), 'true');
  assert.equal(listed?.query.get('extended'), 'true');
  assert.equal(listed?.query.get('per_page'), '25');
});

test('the cursor reads on from each team, and skips the ones read to their end', async () => {
  const { actions, server } = account((call) => {
    if (call.path === '/users/me/teams/t2/threads') return { body: { threads: [entry('old', 10)] } };
    return undefined;
  });
  const page = await actions.threadInbox!('followed', 't2:last');
  assert.deepEqual(page.entries.map((e) => e.root.id), ['old']);
  assert.equal(server.calls.some((call) => call.path === '/users/me/teams/t1/threads'), false);
  const listed = server.calls.find((call) => call.path === '/users/me/teams/t2/threads');
  assert.equal(listed?.query.get('before'), 'last');
  assert.equal(listed?.query.get('unread'), null);
});

test('a deleted root leaves the list', async () => {
  const { actions } = account((call) =>
    call.path.endsWith('/threads') ? { body: { threads: [entry('gone', 10, { post: post('gone', { delete_at: 5 }) })] } } : undefined,
  );
  assert.deepEqual((await actions.threadInbox!('followed', null)).entries, []);
});

test('a page left empty by deleted roots reads on', async () => {
  const gone = Array.from({ length: 25 }, (_, i) => entry(`gone${i}`, 100 - i, { post: post(`gone${i}`, { delete_at: 5 }) }));
  const { actions } = account((call) => {
    if (call.path !== '/users/me/teams/t1/threads') return call.path.endsWith('/threads') ? { body: { threads: [] } } : undefined;
    return { body: { threads: call.query.get('before') === null ? gone : [entry('left', 10)] } };
  });
  const page = await actions.threadInbox!('followed', null);
  assert.deepEqual(page.entries.map((e) => e.root.id), ['left']);
  assert.equal(page.next, null);
});

test('totals count a thread once, whatever the teams listing it', async () => {
  const { actions, server } = account((call) => {
    if (call.path === '/users/me/teams/t1/threads') return { body: { threads: [entry('dm', 90, { unread_mentions: 2 }), entry('r1', 50)] } };
    if (call.path === '/users/me/teams/t2/threads') return { body: { total_unread_threads: 9, threads: [entry('dm', 90, { unread_mentions: 2 })] } };
    return undefined;
  });
  assert.deepEqual(await actions.threadTotals!(), { unreadThreads: 2, unreadMentions: 2 });
  const asked = server.calls.find((call) => call.path === '/users/me/teams/t1/threads');
  assert.deepEqual([asked?.query.get('unread'), asked?.query.get('threadsOnly'), asked?.query.get('per_page')], ['true', 'true', '100']);
});

test('reading a thread goes to its channel team and leaves the channel unread', async () => {
  const { actions, server } = account((call) => (call.method === 'PUT' ? { body: {} } : undefined));
  const before = Date.now();
  await actions.markThreadRead!('ch1', 'r1');
  const put = server.calls.find((call) => call.method === 'PUT');
  const [, at] = put?.path.match(/^\/users\/me\/teams\/t2\/threads\/r1\/read\/(\d+)$/) ?? [];
  assert.ok(Number(at) >= before);
  assert.equal(server.calls.some((call) => call.path === '/channels/members/me/view'), false);
});

test('reading a thread never stops short of its last reply', async () => {
  const { actions, server } = account((call) => (call.method === 'PUT' ? { body: {} } : undefined));
  const ahead = Date.now() + 60_000;
  await actions.markThreadRead!('ch1', 'r1', ahead);
  assert.ok(server.calls.some((call) => call.path === `/users/me/teams/t2/threads/r1/read/${ahead}`));
});

test('a direct message reads through any of my teams', async () => {
  const { actions, server } = account((call) => {
    if (call.path === '/channels/d1') return { body: { id: 'd1', type: 'D', name: 'me__u-bob', team_id: '' } };
    if (call.path === '/channels/d1/members/me') return { body: { channel_id: 'd1', user_id: 'me' } };
    return call.method === 'PUT' ? { body: {} } : undefined;
  });
  await actions.markThreadRead!('d1', 'r1');
  assert.ok(server.calls.some((call) => call.method === 'PUT' && call.path.startsWith('/users/me/teams/t1/threads/r1/read/')));
});

test('marking all reads every team', async () => {
  const { actions, server } = account((call) => (call.method === 'PUT' ? { body: {} } : undefined));
  await actions.markAllThreadsRead!();
  assert.deepEqual(server.calls.filter((call) => call.method === 'PUT').map((call) => call.path), [
    '/users/me/teams/t1/threads/read',
    '/users/me/teams/t2/threads/read',
  ]);
});

test('following is what the thread route answers: 200 yes, 404 no, anything else an error', async () => {
  let status = 200;
  const { actions } = account((call) => (call.path === '/users/me/teams/t2/threads/r1' ? { status, body: status === 200 ? entry('r1', 1) : { id: 'x', message: 'x' } } : undefined));
  assert.equal(await actions.threadFollowing!('ch1', 'r1'), true);
  status = 404;
  assert.equal(await actions.threadFollowing!('ch1', 'r1'), false);
  status = 500;
  await assert.rejects(actions.threadFollowing!('ch1', 'r1'));
});

test('the bell follows and unfollows through its channel team', async () => {
  const { actions, server } = account((call) => (call.path.endsWith('/following') ? { body: {} } : undefined));
  await actions.followThread!('ch1', 'r1', true);
  await actions.followThread!('ch1', 'r1', false);
  assert.deepEqual(server.calls.filter((call) => call.path.endsWith('/following')).map((call) => [call.method, call.path]), [
    ['PUT', '/users/me/teams/t2/threads/r1/following'],
    ['DELETE', '/users/me/teams/t2/threads/r1/following'],
  ]);
});

test('my own gestures say the threads changed, whatever the socket echoes', async () => {
  const { actions } = account((call) => (call.method === 'PUT' ? { body: {} } : undefined));
  let told = 0;
  const release = actions.onThreadsChanged!(() => told++);
  await actions.markThreadRead!('ch1', 'r1');
  await actions.markAllThreadsRead!();
  assert.equal(told, 2);
  release();
  await actions.markAllThreadsRead!();
  assert.equal(told, 2);
});

test('thread events tell the listeners and stay quiet, the thread a string or an object', async () => {
  const server = fakeServer(() => undefined);
  const client = new MmClient(server.base, 'tok', { fetch: server.fetcher });
  const live = new MmLive(client, new MmDirectory(client), 'me');
  let told = 0;
  live.onThreadsChanged(() => told++);
  for (const [name, data] of [
    ['thread_updated', { thread: JSON.stringify({ id: 'r1' }) }],
    ['thread_updated', { thread: { id: 'r1' } }],
    ['thread_read_changed', { thread_id: 'r1' }],
    ['thread_read_changed', { timestamp: 5 }],
    ['thread_follow_changed', { thread_id: 'r1', state: true }],
  ] as const) {
    const events = await live.expand(name, data, {});
    assert.deepEqual(events.map((e) => e.collection), ['mm:quiet']);
  }
  assert.equal(told, 5);
  assert.equal(server.calls.length, 0);
  await live.expand('hello', {}, {});
  assert.equal(told, 5);
});

test('each entry says whether I took part and whether I am mentioned', async () => {
  const dm = (id: string, type: string) => ({
    body: { id, type, name: `${id}__me`, display_name: '', team_id: '' },
  });
  const { actions } = account((call) => {
    if (call.path === '/channels/dm1') return dm('dm1', 'D');
    if (call.path === '/channels/gm1') return dm('gm1', 'G');
    if (call.path === '/channels/dm1/members/me' || call.path === '/channels/gm1/members/me') return { body: { user_id: 'me' } };
    if (call.path === '/users/me/teams/t1/threads') {
      return {
        body: {
          threads: [
            entry('plain', 90),
            entry('mine', 80, { post: post('mine', { user_id: 'me' }) }),
            entry('replied', 70, { participants: [{ id: 'me', username: 'me' }] }),
            entry('direct', 60, { post: post('direct', { channel_id: 'dm1' }) }),
            entry('group', 50, { post: post('group', { channel_id: 'gm1' }) }),
            entry('counted', 40, { unread_mentions: 1 }),
            entry('named', 30, { post: post('named', { message: 'hello @me!' }) }),
            entry('near', 20, { post: post('near', { message: 'hello @menu and @all' }) }),
          ],
        },
      };
    }
    return call.path.endsWith('/threads') ? { body: { threads: [] } } : undefined;
  });
  const { entries } = await actions.threadInbox!('followed', null);
  const scope = Object.fromEntries(entries.map((e) => [e.root.id, [e.participated, e.mentioned]]));
  assert.deepEqual(scope, {
    plain: [false, false],
    mine: [true, false],
    replied: [true, false],
    direct: [true, false],
    group: [true, false],
    counted: [false, true],
    named: [false, true],
    near: [false, false],
  });
});
