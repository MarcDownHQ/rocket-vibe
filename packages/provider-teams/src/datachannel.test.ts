import assert from 'node:assert/strict';
import { describe, test } from 'node:test';

import type { Label } from './calling.ts';
import { Channel, messages, Routing, type Source, type VideoSlot } from './datachannel.ts';

const slot = (streamId: number, label: Label): VideoSlot => ({ streamId, stream: `teams-${streamId}`, label });
const bytes = (frame: number[], json: string) => Uint8Array.from([...frame, ...new TextEncoder().encode(json)]);

describe('Teams data channel', () => {
  test('a request is framed as the web client frames it', () => {
    const channel = new Channel(213);
    const syn = channel.syn();
    assert.deepEqual([...syn.subarray(0, 16)], [0x10, 0x0f, 0x92, 0, 0, 0, 0, 0, 0, 0xd5, 1, 0xff, 0xff, 0xff, 0xfc, 1]);
    assert.equal(new TextDecoder().decode(syn.subarray(16)), '[{"client_capabilities":["dsh","bwe","sr","ssbwe","leave","vdclc","heartbeat"],"type":"syn"}]');
    assert.equal(messages(syn)[0]!.type, 'syn');
    assert.equal(new TextDecoder().decode(channel.heartbeat().subarray(16)), '[{"state":0,"type":"heartbeat"}]');
    const request = channel.subscribe(202, 415, false);
    assert.deepEqual([...request.subarray(4, 6)], [2, 0], 'the frames are counted');
    assert.equal(new TextDecoder().decode(request.subarray(16)),
      '[{"controlVideoStreaming":{"controlInfo":{"fmtParams":[{"max-fps":3000,"max-fs":8160,"max-mbps":244800,"profile-level-id":"64001f"}],"sourceId":415,"streamMsid":202},"sequenceNumber":1},"type":"sr"}]');
    const message = messages(request)[0]! as { type: string; controlVideoStreaming: { sequenceNumber: number; controlInfo: { sourceId: number; streamMsid: number } } };
    assert.equal(message.type, 'sr');
    assert.equal(message.controlVideoStreaming.sequenceNumber, 1);
    assert.equal(message.controlVideoStreaming.controlInfo.sourceId, 415);
    assert.equal(message.controlVideoStreaming.controlInfo.streamMsid, 202);
    const stop = messages(channel.subscribe(202, null, false))[0]! as typeof message;
    assert.equal(stop.controlVideoStreaming.controlInfo.sourceId, -1);
  });
  test('the server\'s frames are read whatever their receivers', () => {
    const frame = bytes([0x10, 0x13, 0x92, 0, 0x56, 0, 0xff, 0xff, 0xff, 0xfc, 2, 0, 0, 0, 0xd5, 0, 0, 1, 0xaa, 1], '[{"history":[414,201],"type":"dsh"}]');
    assert.equal(messages(frame)[0]!.type, 'dsh');
    assert.deepEqual(messages(bytes([0x10, 0x0f, 0x81, 0, 0, 0, 0, 0, 0, 0xd5, 1, 0, 0, 1, 0xaa, 1], '{}')), []);
    assert.deepEqual(messages(Uint8Array.from([0x10])), []);
  });
  test('a live video takes a free section of its kind and leaves it when it stops', () => {
    const slots = [slot(202, 'camera'), slot(203, 'camera'), slot(212, 'screen')];
    const routing = new Routing();
    const camera = (id: number, live: boolean): Source => ({ id, screen: false, live });
    const screen = (id: number, live: boolean): Source => ({ id, screen: true, live });
    const sources = new Map<string, Source[]>();
    sources.set('8:a', [camera(415, false), screen(425, false)]);
    assert.deepEqual(routing.plan(slots, sources), []);
    sources.set('8:a', [camera(415, true), screen(425, true)]);
    assert.deepEqual(routing.plan(slots, sources), [[202, 415, false], [212, 425, true]]);
    assert.deepEqual(routing.plan(slots, sources), [], 'asked once');
    sources.set('8:b', [camera(515, true)]);
    assert.deepEqual(routing.plan(slots, sources), [[203, 515, false]]);
    assert.deepEqual(routing.shown(slots), [
      { stream: 'teams-202', member: '8:a', screen: false },
      { stream: 'teams-203', member: '8:b', screen: false },
      { stream: 'teams-212', member: '8:a', screen: true },
    ]);
    sources.set('8:a', [camera(415, false), screen(425, false)]);
    assert.deepEqual(routing.plan(slots, sources), [[202, null, false], [212, null, true]]);
    sources.delete('8:b');
    assert.deepEqual(routing.plan(slots, sources), [[203, null, false]]);
    assert.deepEqual(routing.shown(slots), []);
  });
});
