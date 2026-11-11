import assert from 'node:assert/strict';
import { describe, test } from 'node:test';
import { carryWhatsAppCall, whatsappCallSignal } from './index.ts';
import type { WaCall } from './frames.ts';

describe('WhatsApp calls', () => {
  test('a 1:1 audio call rings; a video or group call does not, an end stops it', () => {
    const ring: WaCall = { id: 'C1', from: 'a@s.whatsapp.net', state: 'ringing' };
    assert.deepEqual(whatsappCallSignal(ring, 'Ada'), { kind: 'ring', rid: 'a@s.whatsapp.net', messageId: 'C1', callId: 'C1', callerId: 'a@s.whatsapp.net', callerName: 'Ada' });
    assert.equal(whatsappCallSignal({ ...ring, video: true }, null), null);
    assert.equal(whatsappCallSignal({ ...ring, group: 'g@g.us' }, null), null);
    assert.deepEqual(whatsappCallSignal({ ...ring, state: 'ended' }, null), { kind: 'rung-over' });
    assert.equal(whatsappCallSignal({ ...ring, state: 'active' }, null), null);
  });

  const fakeAccount = () => {
    const sent: Record<string, unknown>[] = [];
    let listeners: ((call: WaCall) => void)[] = [];
    return {
      sent,
      push: (call: WaCall) => { for (const l of [...listeners]) l(call); },
      account: {
        command: async (value: Record<string, unknown>) => { sent.push(value); return true; },
        onCall: (l: (call: WaCall) => void) => { listeners.push(l); return () => { listeners = listeners.filter((x) => x !== l); }; },
      },
    };
  };
  const fakeEngine = () => {
    const said: unknown[] = [];
    let hangUp: () => void = () => {};
    let mic: (on: boolean) => void = () => {};
    return {
      said, hangUp: () => hangUp(), mic: (on: boolean) => mic(on),
      engine: {
        connect: async (address: string) => { said.push(['connect', address]); },
        roster: async (members: unknown) => { said.push(['roster', members]); },
        ended: async (reason: string) => { said.push(['ended', reason]); },
        left: () => new Promise<void>((resolve) => { hangUp = resolve; }),
        microphone: (l: (on: boolean) => void) => { mic = l; return () => {}; },
      },
    };
  };

  test('an incoming call is accepted, its audio opened, and ends when the other side hangs up', async () => {
    const a = fakeAccount();
    const e = fakeEngine();
    const carried = carryWhatsAppCall(a.account, 'C1', e.engine, () => 'Ada', () => 'x');
    await new Promise((resolve) => setImmediate(resolve));
    assert.deepEqual(a.sent[0], { type: 'accept_call', id: 'C1' });
    a.push({ id: 'C1', from: 'a@s.whatsapp.net', state: 'connecting', audio: '127.0.0.1:9#t' });
    await carried;
    assert.deepEqual(e.said[0], ['connect', '127.0.0.1:9#t']);
    assert.deepEqual(e.said[1], ['roster', [{ id: 'a@s.whatsapp.net', name: 'Ada', muted: false }]]);
    e.mic(false);
    assert.deepEqual(a.sent.at(-1), { type: 'mute_call', id: 'C1', text: 'on' });
    a.push({ id: 'C1', from: 'a@s.whatsapp.net', state: 'ended' });
    assert.deepEqual(e.said.at(-1), ['ended', 'remote_ended']);
    e.hangUp();
    await new Promise((resolve) => setImmediate(resolve));
    assert.ok(!a.sent.some((v) => v.type === 'hangup_call'), 'over already: nothing to hang up');
  });

  test('a call placed here follows its real id, rosters its peer once active, and a hang-up here hangs up', async () => {
    const a = fakeAccount();
    const e = fakeEngine();
    const carried = carryWhatsAppCall(a.account, 'wa:b@s.whatsapp.net', e.engine, () => 'Bo', () => 'r1');
    await new Promise((resolve) => setImmediate(resolve));
    assert.deepEqual(a.sent[0], { type: 'place_call', id: 'rv-r1', chat: 'b@s.whatsapp.net' });
    a.push({ id: 'rv-r1', from: 'b@s.whatsapp.net', state: 'placed', audio: 'REAL' });
    a.push({ id: 'REAL', from: 'b@s.whatsapp.net', state: 'connecting', audio: '127.0.0.1:9#t' });
    await carried;
    assert.equal(e.said.length, 1, 'no roster until answered');
    a.push({ id: 'REAL', from: 'b@s.whatsapp.net', state: 'active' });
    assert.deepEqual(e.said[1], ['roster', [{ id: 'b@s.whatsapp.net', name: 'Bo', muted: false }]]);
    e.hangUp();
    await new Promise((resolve) => setImmediate(resolve));
    assert.deepEqual(a.sent.at(-1), { type: 'hangup_call', id: 'REAL' });
  });

  test('a call that ends before its audio opens fails', async () => {
    const a = fakeAccount();
    const e = fakeEngine();
    const carried = carryWhatsAppCall(a.account, 'C2', e.engine, () => '', () => 'x');
    await new Promise((resolve) => setImmediate(resolve));
    a.push({ id: 'C2', from: 'a@s.whatsapp.net', state: 'ended', reason: 'not_ringing' });
    await assert.rejects(carried, /not_ringing/);
  });
});
