import assert from 'node:assert/strict';
import { describe, test } from 'node:test';

import { acceptBody, acknowledgeBody, answerExternalCall, attachBody, attachedLinks, audioMid, callPushEvent, isCallCallback, nextSubscription, placeCallBody, placeExternalCall, renegotiationOffer, teamsAnswer, teamsOffer, webrtcAnswer, type Caller } from './calling.ts';

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
    assert.deepEqual(callPushEvent(accepted), { kind: 'accepted', sdp: 'v=0', acknowledge: 'https://cc/ack', callLeg: null });
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
      poll: async () => ({}),
      put: async () => {},
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
      poll: async () => ({}),
      put: async () => {},
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
  test('a new offer keeps the call\'s mid, its answer the offer\'s sections', () => {
    const offer = 'v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 7 8\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\na=inactive\r\na=mid:7\r\nm=video 5 RTP/SAVP 34\r\na=mid:8\r\n';
    const plain = renegotiationOffer(offer, 'v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n')!;
    assert.equal(audioMid(plain), '0');
    assert.ok(plain.includes('a=group:BUNDLE 0\r\n') && plain.includes('a=inactive') && !plain.includes('m=video'));
    const local = 'v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=inactive\r\na=mid:0\r\n';
    const answer = teamsAnswer(local, offer);
    assert.equal(audioMid(answer), '7');
    assert.ok(answer.includes('a=group:BUNDLE 7\r\n') && answer.includes('m=video 0 ') && answer.includes('a=mid:8'));
    assert.equal(nextSubscription({ nextSubscribeUrl: 'https://b/next' }), 'https://b/next');
    assert.equal(nextSubscription({ nextSubscribeUrl: 'http://b/next' }), null);
  });
  test('a new offer leaves out an extension id the call gave another URI', () => {
    const negotiated = 'v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n'
      + 'a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n'
      + 'a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n';
    const offer = 'v=0\r\ns=-\r\nt=0 0\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\na=mid:1\r\n'
      + 'a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n'
      + 'a=extmap:3 http:\\\\skype.com\\experiments\\rtp-hdrext\\fast_bandwidth_feedback#version_3\r\n'
      + 'a=extmap:5 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n'
      + 'a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n';
    const plain = renegotiationOffer(offer, negotiated)!;
    assert.equal(audioMid(plain), '0');
    assert.ok(plain.includes('a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n'));
    assert.ok(plain.includes('a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n'));
    assert.ok(!plain.includes('a=extmap:3 ') && !plain.includes('a=extmap:5 '), plain);
    assert.equal(renegotiationOffer(offer, null)!.split('a=extmap:').length - 1, 4);
  });
  test('a running call holds its lease, registers its leg and answers a hold', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    const posted: [string, unknown][] = [];
    const polled: string[] = [];
    const put: string[] = [];
    const offered: string[] = [];
    let id = 0;
    let leftHere: () => void = () => {};
    await placeExternalCall({
      post: async (url, _chain, body) => { posted.push([url, body]); return url.endsWith('epconv') ? { links: { leave: 'https://conv/leave', subscribe: 'https://b/1', updateEndpointMetadata: 'https://conv/meta' } } : {}; },
      poll: async (url) => { polled.push(url); await new Promise((resolve) => setImmediate(resolve)); return { nextSubscribeUrl: `https://b/${polled.length + 1}` }; },
      put: async (url) => { put.push(url); },
      sleep: async () => {},
      pushes: (listener) => { push = listener; return () => {}; },
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async () => {}, offer: async () => OFFER, answer: async () => {}, roster: async () => {},
      answerOffer: async (sdp) => { offered.push(sdp); return 'v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=inactive\r\na=mid:0\r\n'; },
      ended: async () => {}, left: () => new Promise<void>((resolve) => { leftHere = resolve; }),
    }, ['8:orgid:x'], null);
    const url = (path: string) => `https://t/callAgent/id1/id3/${path}`;
    push({ url: url('call/acceptance/'), body: { callAcceptance: { links: { callLeg: 'https://cc/leg' }, mediaContent: { blob: ANSWER } } } });
    const hold = ANSWER.replace(/a=mid:0/, 'a=mid:9').replace('a=sendrecv', 'a=inactive');
    push({ url: url('call/mediaRenegotiation/'), body: { mediaNegotiation: { mediaContent: { blob: hold, mediaLegId: 'leg1' }, links: { mediaAnswer: 'https://cc/answer' } } } });
    for (let i = 0; i < 5; i++) await new Promise((resolve) => setImmediate(resolve));
    assert.ok(polled.length >= 2 && polled[1] === 'https://b/2', 'the broker is polled again at the link it gave');
    assert.deepEqual(put, ['https://conv/meta']);
    assert.ok(posted.some(([u]) => u === 'https://cc/leg'));
    assert.equal(audioMid(offered[0]!), '0', 'the engine sees the call\'s own mid');
    const answered = posted.find(([u]) => u === 'https://cc/answer')?.[1] as { mediaAnswer: { mediaContent: { blob: string; mediaLegId: string } } };
    assert.equal(answered.mediaAnswer.mediaContent.mediaLegId, 'leg1');
    assert.equal(audioMid(answered.mediaAnswer.mediaContent.blob), '9', 'Teams sees its own mid back');
    leftHere();
    await new Promise((resolve) => setImmediate(resolve));
    const seen = polled.length;
    for (let i = 0; i < 5; i++) await new Promise((resolve) => setImmediate(resolve));
    assert.ok(polled.length <= seen + 1, 'the lease stops with the call');
  });
  test('a ring is answered: attach, the engine\'s answer accepted, then the call runs', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    const posted: [string, Record<string, unknown>][] = [];
    const said: unknown[] = [];
    let id = 0;
    let leftHere: () => void = () => {};
    const offer = ANSWER.replace('a=sendrecv', 'a=sendrecv');
    await answerExternalCall({
      post: async (url, _chain, body) => {
        posted.push([url, body]);
        return url === 'https://cc/attach' ? {
          callInvitation: { links: { acceptance: 'https://cc/accept', subscribe: 'https://b/1' } },
          additionalActionResponses: [{ output: { links: { leave: 'https://conv/leave' } } }],
        } : {};
      },
      poll: () => new Promise(() => {}),
      put: async () => {},
      pushes: (listener) => { push = listener; return () => {}; },
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async (identity) => { said.push(['start', identity]); },
      offer: () => Promise.reject(new Error('answering')),
      answer: async () => {},
      answerOffer: async (sdp) => { said.push(['offered', audioMid(sdp)]); return 'v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\n'; },
      roster: async (members) => { said.push(['roster', members]); },
      ended: async (reason) => { said.push(['ended', reason]); },
      left: () => new Promise<void>((resolve) => { leftHere = resolve; }),
    }, { attach: 'https://cc/attach', offer, mediaLeg: 'LEG', conversation: 'https://conv/c1' });
    assert.equal(posted[0]![0], 'https://cc/attach');
    assert.equal((posted[0]![1].additionalActions as { url: string }[])[0]!.url, 'https://conv/c1');
    assert.deepEqual(said[0], ['start', 'id1']);
    assert.deepEqual(said[1], ['offered', '0']);
    const accept = posted.find(([u]) => u === 'https://cc/accept')?.[1] as { callAcceptance: { mediaContent: { mediaLegId: string; blob: string } } };
    assert.equal(accept.callAcceptance.mediaContent.mediaLegId, 'LEG');
    assert.ok(accept.callAcceptance.mediaContent.blob.includes('m=x-data 0 '), 'the offer\'s other sections refused');
    push({ url: 'https://t/callAgent/id1/id3/call/end/', body: { callEnd: { phrase: 'Remote hangup' } } });
    assert.deepEqual(said.at(-1), ['ended', 'remote_ended']);
    leftHere();
    assert.equal(attachedLinks({}), null);
    assert.equal(attachBody({ mri: 'm', name: 'n', endpoint: 'e', participant: 'p', surl: 'https://t/', random: 'r' }, 'https://c').attach !== undefined, true);
    assert.equal((acceptBody({ mri: 'm', name: 'n', endpoint: 'e', participant: 'p', surl: 'https://t/', random: 'r' }, 'v=0', null).callAcceptance as { acceptedCallModalities: string[] }).acceptedCallModalities[0], 'Audio');
  });
});
