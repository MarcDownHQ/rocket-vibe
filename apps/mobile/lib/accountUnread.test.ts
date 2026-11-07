import assert from 'node:assert/strict';
import { test } from 'node:test';

import { nativeRoomsUnread, serverHost, subscriptionsUnread } from './accountUnread.ts';
import type { Room } from '../providers/vibez/protocol.generated.ts';

test('a Rocket.Chat account is unread like the desktop room list', () => {
  assert.equal(subscriptionsUnread({ update: [] }), 'none');
  assert.equal(subscriptionsUnread({ update: [{ unread: 0, alert: false }] }), 'none');
  assert.equal(subscriptionsUnread({ update: [{ t: 'c', unread: 2 }] }), 'unread');
  assert.equal(subscriptionsUnread({ update: [{ unread: 0, alert: true }] }), 'unread');
  assert.equal(subscriptionsUnread({ update: [{ t: 'd', unread: 3, open: false }] }), 'none', 'a hidden room');
  assert.equal(subscriptionsUnread(null), 'none');
});
test('a Rocket.Chat mention or direct message calls for me, whatever comes after it', () => {
  assert.equal(subscriptionsUnread({ update: [{ t: 'd', unread: 1 }, { t: 'c', unread: 2 }] }), 'attention');
  assert.equal(subscriptionsUnread({ update: [{ t: 'c', unread: 2 }, { t: 'c', unread: 1, userMentions: 1 }] }), 'attention');
  assert.equal(subscriptionsUnread({ update: [{ t: 'd', unread: 0, alert: true }] }), 'unread');
});
test('a Vibez account is unread when a read state has something to read', () => {
  const state = (unread: string, mentions = '0') => ({
    favorite: false, group_mentions: '0', mentions, reply_position: '0', revision: '1',
    room_id: 'r1', root_position: '0', unread_replies: '0', unread_roots: unread,
  });
  const room = (read_state: Room['read_state'], kind: Room['kind'] = 'public'): Room => ({ id: 'r1', kind, name: 'general', revision: '1', read_state });
  assert.equal(nativeRoomsUnread([room(null), room(state('0'))]), 'none');
  assert.equal(nativeRoomsUnread([room(state('0')), room(state('4'))]), 'unread');
  assert.equal(nativeRoomsUnread([room(state('0', '1'))]), 'attention');
  assert.equal(nativeRoomsUnread([room(state('4')), room(state('1'), 'direct')]), 'attention');
});
test('servers are matched by host', () => {
  assert.equal(serverHost('https://Chat.Example.org/'), 'chat.example.org');
  assert.equal(serverHost('http://10.0.2.2:3000'), '10.0.2.2:3000');
  assert.equal(serverHost('chat.example.org'), 'chat.example.org');
});
