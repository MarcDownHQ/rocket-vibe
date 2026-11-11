import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fakeServer } from './testing.ts';
import { mattermostUnread } from './auth.ts';
import type { Session } from '../../lib/auth.ts';

const session = (base: string): Session => ({ kind: 'mattermost', baseUrl: base, userId: 'me', username: 'me', authToken: 'token', siteUrl: null });

function account(type: string, unread: number) {
  return fakeServer((call) => {
    if (call.path === '/users/me/channels') return { body: [{ id: 'c1', type, total_msg_count_root: 10 }] };
    if (call.path === '/users/me/channel_members') return { body: [{ channel_id: 'c1', msg_count_root: 10 - unread, mention_count: 0 }] };
    return undefined;
  });
}

test('an unread direct or group conversation of a closed account asks for attention, a channel only shows unread', async () => {
  for (const [type, unread, expected] of [['O', 0, 'none'], ['O', 2, 'unread'], ['D', 1, 'attention'], ['G', 1, 'attention']] as const) {
    const server = account(type, unread);
    assert.equal(await mattermostUnread(session(server.base), { fetch: server.fetcher }), expected, type);
  }
});
