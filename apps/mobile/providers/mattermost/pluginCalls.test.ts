import assert from 'node:assert/strict';
import { describe, test } from 'node:test';
import { iceServersOf, sessionsOf } from './pluginCalls.ts';
import { pluginCall } from './translator.ts';

describe('Mattermost Calls plugin', () => {
  test('a call post runs until it has an end, its call id its channel\'s', () => {
    assert.deepEqual(pluginCall({ start_at: 1000, end_at: 0 }, 'ch1'), { type: 'videoconf', param: '', conferenceId: 'mmcalls:ch1' });
    assert.deepEqual(pluginCall({ start_at: 1000, end_at: 31_200 }, 'ch1'), { type: 'videoconf-ended', param: '30', conferenceId: null });
    assert.deepEqual(pluginCall({ end_at: 5 }, 'ch1'), { type: 'videoconf-ended', param: '', conferenceId: null });
  });
  test('the plugin\'s servers and people are read', () => {
    assert.deepEqual(iceServersOf([{ urls: ['stun:s:3478'] }, { urls: 'turn:t', username: 'u', credential: 'c' }, {}]), [
      { urls: ['stun:s:3478'], username: '', credential: '' },
      { urls: ['turn:t'], username: 'u', credential: 'c' },
    ]);
    assert.deepEqual(iceServersOf(null), []);
    assert.deepEqual(sessionsOf({ call: { sessions: [{ session_id: 's1', user_id: 'u1' }, { session_id: 's2' }] } }), [['s1', 'u1']]);
    assert.deepEqual(sessionsOf({ channel_id: 'c', enabled: true }), []);
  });
});
