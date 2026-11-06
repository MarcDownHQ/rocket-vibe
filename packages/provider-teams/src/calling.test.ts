import assert from 'node:assert/strict';
import { describe, test } from 'node:test';

import { acknowledgeBody, callPushEvent, isCallCallback, placeCallBody, placeExternalCall, teamsOffer, webrtcAnswer, type Caller } from './calling.ts';

const OFFER = 'v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n'
  + 'm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n'
  + 'a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n'
  + 'a=rtcp:9 IN IP4 0.0.0.0\r\na=rtcp-rsize\r\na=mid:0\r\na=rtpmap:111 opus/48000/2\r\na=ssrc:42 cname:x\r\n'
  + 'a=candidate:1 1 udp 21 2001:db8::1 111 typ host generation 0 ufrag u network-id 1\r\n'
  + 'a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0 ufrag u network-id 1\r\n'
  + 'a=candidate:3 1 tcp 23 192.168.1.5 333 typ host tcptype passive generation 0\r\n';

const ANSWER = 'v=0\r\no=- 455788 0 IN IP4 127.0.0.1\r\ns=session\r\nc=IN IP4 52.1.2.3\r\nb=CT:10000000\r\nt=0 0\r\n'
  + 'a=group:BUNDLE 0 3\r\na=x-plaza-msi-range:1-100 101-200\r\n'
  + 'm=audio 3480 RTP/SAVP 120 111\r\nc=IN IP4 52.1.2.3\r\na=x-signaling-fb:* x-message app send:dsh\r\na=rtcp:3480\r\n'
  + 'a=setup:passive\r\na=ice-ufrag:u\r\na=ice-pwd:p\r\na=rtcp-mux\r\n'
  + 'a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480 MTURNID 154\r\n'
  + 'a=candidate:3 1 tcp-pass 18087935 52.1.2.3 3478 typ relay raddr 10.0.0.1 rport 3478\r\n'
  + 'a=label:main-audio\r\na=mid:0\r\na=rtpmap:111 OPUS/48000/2\r\na=ptime:20\r\na=fingerprint:sha-256 AA\r\n'
  + 'a=extmap:2 http:\\\\www.webrtc.org\\experiments\\rtp-hdrext\\abs-send-time\r\na=x-ssrc-range:1000-1000\r\n'
  + 'm=video 0 RTP/SAVP 102\r\nm=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\na=label:data\r\n';

describe('Teams calling', () => {
  test('an offer speaks the dialect, with a real address', () => {
    const offer = teamsOffer(OFFER);
    assert.ok(offer.includes('m=audio 222 RTP/SAVP 111\r\n'), offer);
    assert.ok(offer.includes('c=IN IP4 192.168.1.5\r\n'));
    assert.ok(offer.includes('a=rtcp:222\r\n'));
    assert.ok(offer.includes('a=extmap:2 http:\\\\www.webrtc.org\\experiments\\rtp-hdrext\\abs-send-time\r\n'), offer);
    assert.ok(offer.includes('a=candidate:2 1 udp 22 192.168.1.5 222 typ host\r\n'));
    assert.ok(offer.includes('a=label:main-audio\r\n'));
    assert.ok(offer.includes('a=x-ssrc-range:42-42\r\n'));
    assert.ok(!offer.includes('tcptype'));
    assert.ok(!offer.includes('rtcp-rsize'));
  });
  test('an answer keeps only plain WebRTC audio', () => {
    const answer = webrtcAnswer(ANSWER)!;
    assert.ok(answer.includes('a=group:BUNDLE 0\r\n'), answer);
    assert.ok(answer.includes('m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n'));
    assert.ok(answer.includes('a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480\r\n'));
    for (const gone of ['MTURNID', 'tcp-pass', 'm=video', 'x-data', 'a=x-', 'a=label']) assert.ok(!answer.includes(gone), gone);
    assert.ok(answer.includes('a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n'), answer);
    assert.equal(webrtcAnswer('v=0\r\nm=video 0 RTP/SAVP 1\r\n'), null);
  });
  test('pushes say what happened', () => {
    const accepted = { url: 'https://x/callAgent/e/r/call/acceptance/', body: JSON.stringify({ callAcceptance: { links: { acknowledgement: 'https://cc/ack' }, mediaContent: { blob: 'v=0' } } }) };
    assert.deepEqual(callPushEvent(accepted), { kind: 'accepted', sdp: 'v=0', acknowledge: 'https://cc/ack' });
    assert.ok(isCallCallback(accepted, 'e'));
    assert.ok(!isCallCallback(accepted, 'other'));
    assert.deepEqual(callPushEvent({ body: { callEnd: { phrase: 'Busy' } } }), { kind: 'ended' });
    assert.deepEqual(callPushEvent({ url: 'https://x/callAgent/e/r/conversation/rosterUpdate/', body: { participants: {
      '8:orgid:a': { details: { displayName: 'Ana' }, endpoints: { x: {} } },
      '8:orgid:b': { details: { displayName: 'Bo' }, endpoints: {} },
    } } }), { kind: 'roster', changes: [{ mri: '8:orgid:a', name: 'Ana', present: true }, { mri: '8:orgid:b', name: 'Bo', present: false }] });
  });
  test('bodies name this client\'s callbacks and the people rung', () => {
    const c: Caller = { mri: '8:orgid:me', name: 'Me', endpoint: 'e', participant: 'p', surl: 'https://t/', random: 'r' };
    const body = placeCallBody(c, ['8:orgid:x'], null, 'v=0', 't') as { groupChat: unknown; participants: { to: unknown[] }; callInvitation: { links: { acceptance: string } } };
    assert.equal(body.groupChat, null);
    assert.deepEqual(body.participants.to, [{ id: '8:orgid:x', participantId: 't' }]);
    assert.equal(body.callInvitation.links.acceptance, 'https://t/callAgent/e/r/call/acceptance/');
    assert.equal((placeCallBody(c, ['8:orgid:x'], '19:t@thread.v2', 'v=0', 't').groupChat as { threadId: string }).threadId, '19:t@thread.v2');
    assert.ok(JSON.stringify(acknowledgeBody(c)).includes('https://t/callAgent/e/r/call/end/'));
  });
  test('a call runs from the offer to its end, and a hang-up here leaves', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    const posted: string[] = [];
    let id = 0;
    const said: unknown[] = [];
    let leftHere: () => void = () => {};
    const engine = {
      start: async (identity: string) => { said.push(['start', identity]); },
      offer: async () => OFFER,
      answer: async (sdp: string) => { said.push(['answer', sdp.includes('m=audio 3480')]); },
      roster: async (members: unknown) => { said.push(['roster', members]); },
      ended: async (reason: string) => { said.push(['ended', reason]); },
      left: () => new Promise<void>((resolve) => { leftHere = resolve; }),
    };
    const deps = {
      post: async (url: string) => { posted.push(url); return url.endsWith('epconv') ? { links: { leave: 'https://conv/leave' } } : {}; },
      pushes: (listener: (envelope: Record<string, unknown>) => void) => { push = listener; return () => { push = () => {}; }; },
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    };
    await placeExternalCall(deps, engine, ['8:orgid:x'], null);
    assert.deepEqual(said[0], ['start', 'id1']);
    assert.ok(posted[0]!.endsWith('/epconv'));
    const url = (path: string) => `https://t/callAgent/id1/id3/${path}`;
    push({ url: 'https://t/callAgent/someone-else/r/call/acceptance/', body: { callAcceptance: { mediaContent: { blob: ANSWER } } } });
    assert.equal(said.length, 1, 'another call\'s push is not this call\'s');
    push({ url: url('call/acceptance/'), body: { callAcceptance: { links: { acknowledgement: 'https://cc/ack' }, mediaContent: { blob: ANSWER } } } });
    assert.deepEqual(said[1], ['answer', true]);
    assert.ok(posted.includes('https://cc/ack'));
    push({ url: url('conversation/rosterUpdate/'), body: { participants: {
      '8:orgid:me': { details: { displayName: 'Me' }, endpoints: { e: {} } },
      '8:orgid:x': { details: { displayName: 'Xo' }, endpoints: { y: {} } },
    } } });
    assert.deepEqual(said[2], ['roster', [{ id: '8:orgid:x', name: 'Xo', muted: false }]]);
    leftHere();
    await new Promise((resolve) => setImmediate(resolve));
    assert.ok(posted.includes('https://conv/leave'));
    assert.equal(said.some((s) => (s as unknown[])[0] === 'ended'), false);
  });
  test('the other side ending ends the call here, with no leave', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    const posted: string[] = [];
    const ended: string[] = [];
    let id = 0;
    await placeExternalCall({
      post: async (url) => { posted.push(url); return { links: { leave: 'https://conv/leave' } }; },
      pushes: (listener) => { push = listener; return () => {}; },
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async () => {}, offer: async () => OFFER, answer: async () => {}, roster: async () => {},
      ended: async (reason) => { ended.push(reason); }, left: () => new Promise<void>(() => {}),
    }, ['8:orgid:x'], null);
    push({ url: 'https://t/callAgent/id1/id3/call/end/', body: { callEnd: { phrase: 'Busy' } } });
    assert.deepEqual(ended, ['remote_ended']);
    assert.ok(!posted.includes('https://conv/leave'));
  });
});
