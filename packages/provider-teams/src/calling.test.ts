import assert from 'node:assert/strict';
import { describe, test } from 'node:test';

import { messages } from './datachannel.ts';
import { acceptBody, acknowledgeBody, answerExternalCall, answerNegotiationBody, attachBody, attachedLinks, callPushEvent, cameraCapabilitiesBody, dataStream, describeMedia, isCallCallback, joinMessage, joinThreadCall, Layout, mediaDescriptions, meetingJoin, modalities, NOT_SENDING, nextSubscription, ownMid, placeCallBody, placeExternalCall, plainAnswer, plainOffer, remoteVideos, renegotiateBody, renegotiationLink, screenSharerBody, sendingSteps, startChannelCallBody, teamsAnswer, teamsOffer, updateMediaDescriptionsBody, videoSlots, type Caller, type Sending } from './calling.ts';

const sectionsOf = (sdp: string) => {
  const out: string[][] = [];
  for (const line of sdp.split('\r\n')) {
    if (line.startsWith('m=')) out.push([line]);
    else if (out.length > 0 && line !== '') out.at(-1)!.push(line);
  }
  return out;
};
const midIn = (section: string[]) => section.find((l) => l.startsWith('a=mid:'))?.slice('a=mid:'.length) ?? null;
const audioMid = (sdp: string) => {
  const audio = sectionsOf(sdp).find((s) => s[0]!.startsWith('m=audio '));
  return audio === undefined ? null : midIn(audio);
};
const section = (sdp: string, mid: string) => sectionsOf(sdp).find((s) => midIn(s) === mid)?.join('\n') ?? '';
const settle = async () => { for (let i = 0; i < 10; i++) await new Promise((resolve) => setImmediate(resolve)); };

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

/** An offer of Teams' with the other side's camera on and its screen shared. */
const VIDEO_OFFER = 'v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2 3\r\n'
  + 'm=audio 3480 RTP/SAVP 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\na=sendrecv\r\na=label:main-audio\r\n'
  + 'm=video 3480 RTP/SAVP 102 103\r\nc=IN IP4 52.1.2.3\r\na=x-signaling-fb:* x-message app send:src recv:src,vc\r\n'
  + 'a=mid:1\r\na=msid:abc def\r\na=sendrecv\r\na=rtcp-rsize\r\na=rtpmap:102 H264/90000\r\na=rtpmap:103 rtx/90000\r\n'
  + 'a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r\na=ssrc:7 cname:c\r\na=ssrc:7 msid:abc def\r\na=label:main-video\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=mid:2\r\na=sendonly\r\na=rtpmap:102 H264/90000\r\n'
  + 'a=label:applicationsharing-video\r\n'
  + 'm=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\na=sendrecv\r\na=label:data\r\n';

/** libwebrtc's answer to that offer made plain. */
const VIDEO_ANSWER = 'v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2\r\n'
  + 'm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendrecv\r\na=ssrc:42 cname:x\r\n'
  + 'a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102 103\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=msid:ep-camera cam\r\na=sendrecv\r\n'
  + 'a=rtcp-rsize\r\na=ssrc:51 cname:x\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\na=recvonly\r\n';

/** This side's new offer: its camera on the section that receives the other side's, its screen on one of its own. */
const OURS = 'v=0\r\no=- 1 3 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2 6\r\n'
  + 'm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendrecv\r\n'
  + 'a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=msid:ep-camera cam\r\na=sendrecv\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\na=recvonly\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:6\r\na=msid:ep-screen scr\r\na=sendonly\r\n'
  + 'a=ssrc:61 cname:x\r\n';

/** Teams' answer to it: the other side's camera still on, this side's screen taken. */
const OURS_ANSWERED = 'v=0\r\no=- 9 9 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 6\r\n'
  + 'm=audio 3480 RTP/SAVP 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\na=sendrecv\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=mid:1\r\na=sendonly\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=mid:6\r\na=recvonly\r\n';

describe('Teams calling', () => {
  test('an offer speaks the dialect, with a real address', () => {
    const layout = new Layout();
    const offer = teamsOffer(OFFER, NOT_SENDING, layout);
    assert.ok(offer.includes('m=audio 222 RTP/SAVP 111\r\n'), offer);
    assert.ok(offer.includes('c=IN IP4 192.168.1.5\r\n'));
    assert.ok(offer.includes('a=rtcp:222\r\n'));
    assert.ok(offer.includes('a=extmap:2 http:\\\\www.webrtc.org\\experiments\\rtp-hdrext\\abs-send-time\r\n'), offer);
    assert.ok(offer.includes('a=candidate:2 1 udp 22 192.168.1.5 222 typ host\r\n'));
    assert.ok(offer.includes('a=label:main-audio\r\n'));
    assert.ok(offer.includes('a=x-ssrc-range:42-42\r\n'));
    assert.ok(!offer.includes('tcptype'));
    assert.ok(!offer.includes('rtcp-rsize'));
    assert.ok(layout.offering());
  });
  test('an answer keeps only plain WebRTC audio', () => {
    const layout = new Layout();
    assert.equal(plainAnswer(ANSWER, layout), null, 'no offer waits');
    teamsOffer(OFFER, NOT_SENDING, layout);
    const answer = plainAnswer(ANSWER, layout)!;
    assert.ok(!layout.offering());
    assert.ok(answer.includes('a=group:BUNDLE 0\r\n'), answer);
    assert.ok(answer.includes('m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n'));
    assert.ok(answer.includes('a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480\r\n'));
    for (const gone of ['MTURNID', 'tcp-pass', 'm=video', 'x-data', 'a=x-', 'a=label']) assert.ok(!answer.includes(gone), gone);
    assert.ok(answer.includes('a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n'), answer);
    teamsOffer(OFFER, NOT_SENDING, layout);
    assert.equal(plainAnswer('v=0\r\nm=video 0 RTP/SAVP 1\r\n', layout), null);
  });
  test('pushes say what happened', () => {
    const accepted = { url: 'https://x/callAgent/e/r/call/acceptance/', body: JSON.stringify({ callAcceptance: { links: { acknowledgement: 'https://cc/ack', mediaRenegotiation: 'https://cc/active/1/renegotiate' }, mediaContent: { blob: 'v=0' } } }) };
    assert.deepEqual(callPushEvent(accepted), { kind: 'accepted', sdp: 'v=0', acknowledge: 'https://cc/ack', callLeg: null, renegotiation: 'https://cc/active/1/renegotiate', mediaDescriptions: null, channelParameters: null });
    assert.equal(renegotiationLink({ callAcceptanceAcknowledgement: { links: { mediaRenegotiation: 'https://cc/r' } } }), 'https://cc/r');
    assert.equal(renegotiationLink({ links: { mediaRenegotiation: 'wss://x' } }), null);
    assert.ok(isCallCallback(accepted, 'e'));
    assert.ok(!isCallCallback(accepted, 'other'));
    assert.deepEqual(callPushEvent({ body: { callEnd: { phrase: 'Busy' } } }), { kind: 'ended' });
    assert.deepEqual(callPushEvent({ url: 'https://x/callAgent/e/r/conversation/rosterUpdate/', body: { participants: {
      '8:orgid:a': { details: { displayName: 'Ana' }, endpoints: { x: { call: { mediaStreams: [
        { type: 'audio', label: 'main-audio', sourceId: 414, direction: 'sendrecv' },
        { type: 'video', label: 'main-video', sourceId: 415, direction: 'sendrecv' },
        { type: 'applicationsharing-video', label: 'applicationsharing-video', sourceId: 425, direction: 'recvonly' },
        { type: 'data', label: 'data', sourceId: 426, direction: 'sendrecv' },
      ] } } } },
      '8:orgid:b': { details: { displayName: 'Bo' }, endpoints: {} },
    } } }), { kind: 'roster', changes: [
      { mri: '8:orgid:a', name: 'Ana', present: true, sources: [{ id: 415, screen: false, live: true }, { id: 425, screen: true, live: false }] },
      { mri: '8:orgid:b', name: 'Bo', present: false, sources: [] },
    ] });
  });
  test('bodies name this client\'s callbacks and the people rung', () => {
    const c: Caller = { mri: '8:orgid:me', name: 'Me', endpoint: 'e', participant: 'p', surl: 'https://t/', random: 'r' };
    const body = placeCallBody(c, ['8:orgid:x'], null, 'v=0', 't') as { groupChat: unknown; participants: { to: unknown[] }; callInvitation: { links: { acceptance: string } } };
    assert.equal(body.groupChat, null);
    assert.deepEqual(body.participants.to, [{ id: '8:orgid:x', participantId: 't' }]);
    assert.equal(body.callInvitation.links.acceptance, 'https://t/callAgent/e/r/call/acceptance/');
    assert.equal((placeCallBody(c, ['8:orgid:x'], '19:t@thread.v2', 'v=0', 't').groupChat as { threadId: string }).threadId, '19:t@thread.v2');
    assert.ok(JSON.stringify(acknowledgeBody(c)).includes('https://t/callAgent/e/r/call/end/'));
    const renegotiate = renegotiateBody(c, 'v=1', 'LEG', ['Audio', 'ScreenSharer'], true) as { mediaNegotiation: { callModalities: string[]; links: { mediaAnswer: string }; mediaContent: { blob: string; mediaLegId: string; negotiationTag: string } } };
    assert.equal(renegotiate.mediaNegotiation.mediaContent.negotiationTag, `${c.participant};ss_1`);
    assert.equal(renegotiate.mediaNegotiation.links.mediaAnswer, 'https://t/callAgent/e/r/call/mediaAnswer/');
    assert.equal(renegotiate.mediaNegotiation.mediaContent.blob, 'v=1');
    assert.equal(renegotiate.mediaNegotiation.mediaContent.mediaLegId, 'LEG');
    assert.deepEqual(renegotiate.mediaNegotiation.callModalities, ['Audio', 'ScreenSharer']);
    assert.deepEqual((acceptBody(c, 'v=0', 'LEG', ['Audio', 'Video']).callAcceptance as { acceptedCallModalities: string[] }).acceptedCallModalities, ['Audio', 'Video']);
    assert.deepEqual((answerNegotiationBody('v=0', null, ['Audio', 'ScreenViewer']).mediaAnswer as { callModalities: string[] }).callModalities, ['Audio', 'ScreenViewer']);
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
    await settle();
    assert.deepEqual(said[1], ['answer', true]);
    assert.ok(posted.includes('https://cc/ack'));
    push({ url: url('conversation/rosterUpdate/'), body: { participants: {
      '8:orgid:me': { details: { displayName: 'Me' }, endpoints: { e: {} } },
      '8:orgid:x': { details: { displayName: 'Xo' }, endpoints: { y: {} } },
    } } });
    await settle();
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
    const offer = 'v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 7 8\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\na=inactive\r\na=mid:7\r\nm=video 0 RTP/SAVP 34\r\na=mid:8\r\n';
    const layout = new Layout();
    layout.remote = 'v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n';
    const plain = plainOffer(offer, layout)!;
    assert.equal(audioMid(plain), '0');
    assert.ok(plain.includes('a=group:BUNDLE 0\r\n') && plain.includes('a=inactive'));
    assert.ok(section(plain, '8').startsWith('m=video 0 '), plain);
    const local = 'v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=inactive\r\na=mid:0\r\n';
    const answer = teamsAnswer(local, offer, NOT_SENDING, layout);
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
    const layout = new Layout();
    layout.remote = negotiated;
    const plain = plainOffer(offer, layout)!;
    assert.equal(audioMid(plain), '0');
    assert.ok(plain.includes('a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n'));
    assert.ok(plain.includes('a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n'));
    assert.ok(!plain.includes('a=extmap:3 ') && !plain.includes('a=extmap:5 '), plain);
    assert.equal(plainOffer(offer, new Layout())!.split('a=extmap:').length - 1, 4);
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
    assert.equal((acceptBody({ mri: 'm', name: 'n', endpoint: 'e', participant: 'p', surl: 'https://t/', random: 'r' }, 'v=0', null, ['Audio']).callAcceptance as { acceptedCallModalities: string[] }).acceptedCallModalities[0], 'Audio');
  });
  test('an offer from Teams is answered in its order', () => {
    const offer = 'v=0\r\nt=0 0\r\nm=audio 3480 RTP/SAVP 120 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\n'
      + 'm=video 0 RTP/SAVP 102\r\na=mid:1\r\nm=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\n';
    const layout = new Layout();
    assert.ok(plainOffer(offer, layout)!.includes('m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n'));
    const local = 'v=0\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n'
      + 'a=mid:0\r\na=sendonly\r\na=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n';
    const answer = teamsAnswer(local, offer, NOT_SENDING, layout);
    assert.ok(answer.includes('m=audio 222 RTP/SAVP 111\r\n'), answer);
    assert.ok(answer.includes('a=sendonly\r\n'));
    assert.ok(answer.includes('m=video 0 RTP/SAVP 102\r\na=mid:1\r\n'), answer);
    assert.ok(answer.includes('m=x-data 0 RTP/SAVP 127\r\na=mid:3\r\n'), answer);
    assert.ok(answer.indexOf('m=audio') < answer.indexOf('m=video'));
  });
  test('a section libwebrtc takes no media on keeps its mid out of this side\'s reach', () => {
    const layout = new Layout();
    const plain = plainOffer(VIDEO_OFFER, layout)!;
    assert.equal(section(plain, '3'), 'm=application 0 UDP/DTLS/SCTP webrtc-datachannel\nc=IN IP4 0.0.0.0\na=mid:3', plain);
    assert.ok(plain.includes('a=group:BUNDLE 0 1 2\r\n'), plain);
    const again = plainOffer(VIDEO_OFFER, layout)!;
    assert.equal(again.split('a=mid:3\r\n').length - 1, 1, again);
    assert.ok(again.indexOf('m=application') > again.indexOf('a=mid:2'));
  });

  test('the other side\'s camera and screen are received under streams of their own', () => {
    const layout = new Layout();
    const plain = plainOffer(VIDEO_OFFER, layout)!;
    assert.ok(plain.includes('a=group:BUNDLE 0 1 2\r\n'), plain);
    assert.ok(plain.includes('m=video 3480 UDP/TLS/RTP/SAVPF 102 103\r\n'), plain);
    const camera = section(plain, '1');
    assert.ok(camera.includes('a=msid:teams-1 teams-1') && !camera.includes('abc'), camera);
    assert.ok(camera.includes('a=ssrc:7 cname:c') && camera.includes('a=rtcp-rsize'), camera);
    assert.ok(!plain.includes('x-data') && !plain.includes('a=label') && !plain.includes('a=x-'), plain);
    assert.deepEqual(remoteVideos(layout), [['teams-1', 'camera'], ['teams-2', 'screen']]);
    assert.deepEqual(modalities(layout, NOT_SENDING), ['Audio', 'Video', 'ScreenViewer']);
    const answer = teamsAnswer(VIDEO_ANSWER, VIDEO_OFFER, NOT_SENDING, layout);
    assert.ok(answer.includes('a=group:BUNDLE 0 1 2\r\n'), answer);
    const answered = section(answer, '1');
    assert.ok(answered.startsWith('m=video 222 RTP/SAVP 102 103'), answered);
    assert.ok(answered.includes('a=label:main-video') && answered.includes('a=recvonly'), `camera off: ${answered}`);
    assert.ok(answered.includes('a=x-ssrc-range:51-51') && answered.includes('a=rtcp-rsize'), answered);
    assert.ok(section(answer, '2').includes('a=label:applicationsharing-video'), answer);
    assert.ok(answer.includes('m=x-data 0 RTP/SAVP 127\r\na=mid:3\r\n'), answer);
    const on = teamsAnswer(VIDEO_ANSWER, VIDEO_OFFER, { camera: true, screen: false }, layout);
    assert.ok(section(on, '1').includes('a=sendrecv'), on);
  });
  test('a video section Teams renumbers becomes a new one', () => {
    const layout = new Layout();
    plainOffer(VIDEO_OFFER, layout);
    const moved = VIDEO_OFFER.replace('a=mid:2\r\n', 'a=mid:5\r\n').replace('BUNDLE 0 1 2 3', 'BUNDLE 0 1 5 3');
    const plain = plainOffer(moved, layout)!;
    assert.ok(plain.includes('a=group:BUNDLE 0 1 5\r\n'), plain);
    assert.ok(section(plain, '2').startsWith('m=video 0 '), `the old one refused: ${plain}`);
    assert.ok(section(plain, '2').includes('a=inactive'), plain);
    assert.ok(plain.indexOf('a=mid:2') < plain.indexOf('a=mid:5'), `in its place: ${plain}`);
    assert.deepEqual(remoteVideos(layout), [['teams-1', 'camera'], ['teams-5', 'screen']]);
    const answered = VIDEO_ANSWER.replace('BUNDLE 0 1 2', 'BUNDLE 0 1 5').replace(
      'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\n',
      'm=video 0 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=mid:5\r\n',
    );
    const answer = teamsAnswer(answered, moved, NOT_SENDING, layout);
    assert.ok(section(answer, '5').includes('a=label:applicationsharing-video'), answer);
    assert.ok(!answer.includes('a=mid:2'), answer);
  });
  test('this side\'s screen takes the label over, and a stopped one says so', () => {
    const layout = new Layout();
    plainOffer(VIDEO_OFFER, layout);
    const sending = { camera: false, screen: true };
    const offer = teamsOffer(OURS, sending, layout);
    assert.ok(offer.includes('a=group:BUNDLE 0 1 6\r\n'), offer);
    assert.ok(!offer.includes('a=mid:2'), `the screen it received gives way: ${offer}`);
    assert.ok(section(offer, '6').includes('a=label:applicationsharing-video'), offer);
    assert.ok(section(offer, '6').includes('a=sendonly') && section(offer, '6').includes('x-ssrc-range:61-61'));
    assert.ok(section(offer, '1').includes('a=recvonly'), `a camera sending nothing: ${offer}`);
    assert.deepEqual(modalities(layout, sending), ['Audio', 'Video', 'ScreenSharer']);
    const plain = plainAnswer(OURS_ANSWERED, layout)!;
    assert.ok(plain.includes('a=group:BUNDLE 0 1 6\r\n'), plain);
    assert.ok(section(plain, '2').startsWith('m=video 0 '), plain);
    assert.ok(section(plain, '1').includes('a=msid:teams-1 teams-1'), plain);
    assert.ok(!section(plain, '6').includes('a=msid'), plain);
    assert.deepEqual(remoteVideos(layout), [['teams-1', 'camera']]);
    const stopped = teamsOffer(OURS, NOT_SENDING, layout);
    assert.ok(section(stopped, '6').includes('a=inactive'), stopped);
  });
  test('a placed call offers its camera once Teams took it, on the link its acceptance names', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    let offerAnew: (sdp: string) => void = () => {};
    const posted: [string, Record<string, unknown>][] = [];
    const answers: string[] = [];
    let id = 0;
    let unsubscribed = false;
    await placeExternalCall({
      post: async (url, _chain, body) => { posted.push([url, body]); return url.endsWith('epconv') ? { links: { leave: 'https://conv/leave' } } : {}; },
      poll: async () => ({}),
      put: async () => {},
      pushes: (listener) => { push = listener; return () => {}; },
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async () => {}, offer: async () => OFFER, roster: async () => {},
      answer: async (sdp) => { answers.push(sdp); },
      ended: async () => {}, left: () => new Promise<void>(() => {}),
      offers: (listener) => { offerAnew = listener; return () => { unsubscribed = true; }; },
      sending: () => ({ camera: true, screen: false }),
    }, ['8:orgid:x'], null);
    const url = (path: string) => `https://t/callAgent/id1/id3/${path}`;
    offerAnew(OURS);
    await settle();
    assert.ok(!posted.some(([u]) => u === 'https://cc/reneg'), 'not before Teams took the call');
    push({ url: url('call/acceptance/'), body: { callAcceptance: { links: { acknowledgement: 'https://cc/ack', mediaRenegotiation: 'https://cc/reneg' }, mediaContent: { blob: ANSWER } } } });
    await settle();
    assert.equal(answers.length, 1);
    const offered = posted.find(([u]) => u === 'https://cc/reneg')?.[1] as { mediaNegotiation: { callModalities: string[]; mediaContent: { blob: string } } };
    assert.ok(offered !== undefined, 'the offer that waited goes out');
    assert.deepEqual(offered.mediaNegotiation.callModalities, ['Audio', 'Video']);
    assert.ok(section(offered.mediaNegotiation.mediaContent.blob, '1').includes('a=label:main-video'));
    push({ url: url('call/end/'), body: { callEnd: {} } });
    await settle();
    assert.ok(unsubscribed, 'the offers stop with the call');
  });
  test('a call answered here offers its camera anew and takes Teams\' mediaAnswer', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    let offerAnew: (sdp: string) => void = () => {};
    const posted: [string, Record<string, unknown>][] = [];
    const answers: string[] = [];
    const shown: unknown[] = [];
    let id = 0;
    await answerExternalCall({
      post: async (url, _chain, body) => {
        posted.push([url, body]);
        if (url === 'https://cc/attach') {
          return {
            callInvitation: { links: { acceptance: 'https://cc/accept' } },
            additionalActionResponses: [{ output: { links: { leave: 'https://conv/leave' } } }],
          };
        }
        return url === 'https://cc/accept' ? { callAcceptanceAcknowledgement: { links: { mediaRenegotiation: 'https://cc/reneg' } } } : {};
      },
      poll: () => new Promise(() => {}),
      put: async () => {},
      pushes: (listener) => { push = listener; return () => {}; },
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async () => {}, offer: () => Promise.reject(new Error('answering')), roster: async () => {},
      answer: async (sdp) => { answers.push(sdp); },
      answerOffer: async () => VIDEO_ANSWER,
      ended: async () => {}, left: () => new Promise<void>(() => {}),
      offers: (listener) => { offerAnew = listener; return () => {}; },
      sending: () => ({ camera: true, screen: false }),
      videos: async (videos) => { shown.push(videos); },
    }, { attach: 'https://cc/attach', offer: VIDEO_OFFER, mediaLeg: 'LEG', conversation: 'https://conv/c1' });
    const accept = posted.find(([u]) => u === 'https://cc/accept')![1] as { callAcceptance: { acceptedCallModalities: string[]; mediaContent: { blob: string } } };
    assert.deepEqual(accept.callAcceptance.acceptedCallModalities, ['Audio', 'Video', 'ScreenViewer']);
    assert.ok(section(accept.callAcceptance.mediaContent.blob, '1').includes('a=sendrecv'), 'the camera sends');
    const url = (path: string) => `https://t/callAgent/id1/id3/${path}`;
    push({ url: url('conversation/rosterUpdate/'), body: { participants: { '8:orgid:x': { details: { displayName: 'Xo' }, endpoints: { y: {} } } } } });
    await settle();
    assert.deepEqual(shown.at(-1), [{ stream: 'teams-1', member: '8:orgid:x', screen: false }, { stream: 'teams-2', member: '8:orgid:x', screen: true }]);
    offerAnew(OURS);
    await settle();
    const offered = posted.find(([u]) => u === 'https://cc/reneg')?.[1] as { mediaNegotiation: { callModalities: string[]; mediaContent: { blob: string; mediaLegId: string } } };
    assert.ok(offered !== undefined, 'the new offer goes to the link the acceptance came back with');
    assert.equal(offered.mediaNegotiation.mediaContent.mediaLegId, 'LEG');
    assert.ok(section(offered.mediaNegotiation.mediaContent.blob, '6').includes('a=label:applicationsharing-video'));
    push({ url: url('call/mediaAnswer/'), body: { mediaAnswer: { links: { mediaAcknowledgement: 'https://cc/ack2' }, mediaContent: { blob: OURS_ANSWERED } } } });
    await settle();
    assert.equal(answers.length, 1);
    assert.ok(section(answers[0]!, '1').includes('a=msid:teams-1 teams-1'), answers[0]);
    assert.deepEqual(posted.find(([u]) => u === 'https://cc/ack2')?.[1], {}, 'the answer is acknowledged with an empty body');
    assert.deepEqual(shown.at(-1), [{ stream: 'teams-1', member: '8:orgid:x', screen: false }]);
    offerAnew(OURS);
    await settle();
    push({ url: url('call/mediaRenegotiation/'), body: { mediaNegotiation: { mediaContent: { blob: VIDEO_OFFER, mediaLegId: 'LEG2' }, links: { mediaAnswer: 'https://cc/answer' } } } });
    await settle();
    push({ url: url('call/mediaAnswer/'), body: { mediaAnswer: { mediaContent: { blob: OURS_ANSWERED } } } });
    await settle();
    assert.equal(answers.length, 1, 'an answer to an offer Teams\' own withdrew is dropped');
    const negotiated = posted.find(([u]) => u === 'https://cc/answer')?.[1] as { mediaAnswer: { mediaContent: { mediaLegId: string } } };
    assert.equal(negotiated.mediaAnswer.mediaContent.mediaLegId, 'LEG2');
  });
});

const GROUP_OFFER = 'v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2 3 4\r\n'
  + 'm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendrecv\r\na=ssrc:42 cname:x\r\n'
  + 'a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n'
  + 'm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:uf\r\na=ice-pwd:pw\r\n'
  + 'a=fingerprint:sha-256 AB\r\na=setup:actpass\r\na=mid:1\r\na=sctp-port:5000\r\na=max-message-size:262144\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\na=recvonly\r\na=rtpmap:102 H264/90000\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:3\r\na=recvonly\r\na=rtpmap:102 H264/90000\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:4\r\na=recvonly\r\na=rtpmap:102 H264/90000\r\n';

const GROUP_ANSWER = 'v=0\r\no=- 1 0 IN IP4 127.0.0.1\r\ns=session\r\nc=IN IP4 52.1.2.3\r\nt=0 0\r\n'
  + 'm=audio 3480 RTP/SAVP 111\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:201\r\na=setup:passive\r\n'
  + 'a=ice-ufrag:Z6\r\na=ice-pwd:pw2\r\na=label:main-audio\r\na=mid:0\r\na=rtpmap:111 OPUS/48000/2\r\n'
  + 'm=x-data 3480 RTP/SAVP 127 126\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:205\r\na=setup:passive\r\n'
  + 'a=ice-ufrag:Z6\r\na=ice-pwd:pw2\r\na=rtcp-mux\r\na=label:data\r\na=mid:1\r\na=rtpmap:127 x-data/90000\r\n'
  + 'a=fingerprint:sha-256 CD\r\na=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480 MTURNID 9\r\n'
  + 'a=x-data-protocol:sctp\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:202\r\na=label:main-video\r\na=mid:2\r\n'
  + 'a=sendonly\r\na=rtpmap:102 H264/90000\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:203\r\na=label:main-video\r\na=mid:3\r\n'
  + 'a=sendonly\r\na=rtpmap:102 H264/90000\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:204\r\na=label:applicationsharing-video\r\n'
  + 'a=mid:4\r\na=sendonly\r\na=rtpmap:102 H264/90000\r\n';

describe('Teams group calls', () => {
  test('a group call offers every video section, the last for a screen, and its data channel', () => {
    const layout = Layout.group();
    const offer = teamsOffer(GROUP_OFFER, NOT_SENDING, layout);
    assert.ok(offer.includes('a=group:BUNDLE 0 1 2 3 4\r\n'), offer);
    assert.equal(offer.split('a=label:main-video').length - 1, 2, offer);
    assert.ok(section(offer, '4').includes('a=label:applicationsharing-video'), offer);
    let data = section(offer, '1');
    assert.ok(data.startsWith('m=x-data 222 RTP/SAVP 127\n'), data);
    for (const line of ['a=x-data-protocol:sctp', 'a=rtpmap:127 x-data/90000', 'a=setup:actpass', 'a=ice-ufrag:uf']) assert.ok(data.includes(line), `${line}: ${data}`);
    assert.ok(data.includes('a=label:data') && !data.includes('sctp-port'), data);
    assert.ok(section(offer, '2').includes('a=x-ssrc-range:1-1'), 'a receiving section names no source');

    const answer = plainAnswer(GROUP_ANSWER, layout)!;
    data = section(answer, '1');
    assert.ok(data.startsWith('m=application 3480 UDP/DTLS/SCTP webrtc-datachannel\n'), data);
    assert.ok(data.includes('a=sctp-port:5000') && data.includes('a=setup:passive'), data);
    assert.ok(data.includes('a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480\n'), data);
    assert.ok(answer.includes('a=group:BUNDLE 0 1 2 3 4\r\n'), answer);
    assert.equal(dataStream(layout), 205);
    assert.deepEqual(videoSlots(layout), [
      { streamId: 202, stream: 'teams-2', label: 'camera' },
      { streamId: 203, stream: 'teams-3', label: 'camera' },
      { streamId: 204, stream: 'teams-4', label: 'screen' },
    ]);

    const again = teamsOffer(GROUP_OFFER, NOT_SENDING, layout);
    assert.ok(section(again, '4').includes('a=label:applicationsharing-video'), 'the screen\'s section stays');
    assert.ok(section(again, '3').includes('a=label:main-video'), again);
  });
  test('a one-to-one call still offers one camera section', () => {
    const offer = teamsOffer(GROUP_OFFER, NOT_SENDING, new Layout());
    assert.equal(offer.split('m=video').length - 1, 1, offer);
    assert.ok(!offer.includes('applicationsharing'), offer);
  });
  test('a group call greets the media server, beats, asks for each live video and names it, and stops beating when it ends', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    let id = 0;
    const started: unknown[] = [];
    const sent: Uint8Array[] = [];
    const shown: unknown[] = [];
    const posted: string[] = [];
    await joinThreadCall({
      post: async (url) => { posted.push(url); return { links: { leave: 'https://conv/leave' } }; },
      pushes: (listener) => { push = listener; return () => { push = () => {}; }; },
      poll: async () => ({}),
      put: async () => {},
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async (_identity, options) => { started.push(options); },
      offer: async () => GROUP_OFFER,
      answer: async () => {},
      roster: async () => {},
      ended: async () => {},
      left: () => new Promise<void>(() => {}),
      sendData: async (data) => { sent.push(data); },
      videos: async (videos) => { shown.push(videos); },
    }, '19:c@thread.tacv2', null);
    assert.deepEqual(started, [{ dataChannel: 'main-channel', receiveVideos: 3, videoUpFront: true }]);
    const url = (path: string) => `https://t/callAgent/id1/id3/${path}`;
    push({ url: url('call/acceptance/'), body: { callAcceptance: { mediaContent: { blob: GROUP_ANSWER } } } });
    await settle();
    assert.equal(messages(sent[0]!)[0]!.type, 'syn');
    assert.deepEqual([...sent[0]!.subarray(6, 10)], [0, 0, 0, 205], 'this side speaks from the data section\'s stream id');
    push({ url: url('conversation/rosterUpdate/'), body: { participants: {
      '8:orgid:me': { details: { displayName: 'Me' }, endpoints: { e: { call: { mediaStreams: [{ label: 'main-video', sourceId: 1, direction: 'sendrecv' }] } } } },
      '8:orgid:a': { details: { displayName: 'Ana' }, endpoints: { x: { call: { mediaStreams: [
        { label: 'main-video', sourceId: 415, direction: 'sendrecv' },
        { label: 'applicationsharing-video', sourceId: 425, direction: 'sendonly' },
      ] } } } },
    } } });
    await settle();
    const asked = sent.slice(1).map((frame) => (messages(frame)[0]!.controlVideoStreaming as { controlInfo: unknown }).controlInfo);
    assert.deepEqual(asked.map((info) => [(info as { streamMsid: number }).streamMsid, (info as { sourceId: number }).sourceId]), [[202, 415], [204, 425]]);
    assert.deepEqual(shown.at(-1), [
      { stream: 'teams-2', member: '8:orgid:a', screen: false },
      { stream: 'teams-4', member: '8:orgid:a', screen: true },
    ]);
    const before = sent.length;
    await new Promise((resolve) => setTimeout(resolve, 1100));
    assert.equal(messages(sent[before]!)[0]!.type, 'heartbeat');
    push({ url: url('conversation/end/'), body: { conversationEnd: {} } });
    await settle();
    const after = sent.length;
    await new Promise((resolve) => setTimeout(resolve, 1100));
    assert.equal(sent.length, after, 'no beat once the call ended');
  });
});

/** libwebrtc's offer for a group call whose camera and screen sections come up front, no track on either yet: then the data channel and two receivers. */
const UP_FRONT_OFFER = 'v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2 3 4 5\r\n'
  + 'm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendrecv\r\na=ssrc:42 cname:x\r\n'
  + 'a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102 103\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=sendrecv\r\na=msid:ep-camera t1\r\n'
  + 'a=rtpmap:102 H264/90000\r\na=rtpmap:103 rtx/90000\r\na=ssrc-group:FID 51 52\r\na=ssrc:51 cname:x\r\n'
  + 'a=ssrc:51 msid:ep-camera t1\r\na=ssrc:52 cname:x\r\na=ssrc:52 msid:ep-camera t1\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\na=sendrecv\r\na=msid:ep-screen t2\r\n'
  + 'a=rtpmap:102 H264/90000\r\na=ssrc:61 cname:x\r\na=ssrc:61 msid:ep-screen t2\r\n'
  + 'm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:uf\r\na=ice-pwd:pw\r\n'
  + 'a=fingerprint:sha-256 AB\r\na=setup:actpass\r\na=mid:3\r\na=sctp-port:5000\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:4\r\na=recvonly\r\na=rtpmap:102 H264/90000\r\n'
  + 'm=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:5\r\na=recvonly\r\na=rtpmap:102 H264/90000\r\n';

/** The media server's answer: it sends on every video section, this side's own included. */
const UP_FRONT_ANSWER = 'v=0\r\no=- 1 0 IN IP4 127.0.0.1\r\ns=session\r\nc=IN IP4 52.1.2.3\r\nt=0 0\r\n'
  + 'm=audio 3480 RTP/SAVP 111\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:201\r\na=label:main-audio\r\na=mid:0\r\n'
  + 'a=rtpmap:111 OPUS/48000/2\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:202\r\na=label:main-video\r\na=mid:1\r\n'
  + 'a=rtpmap:102 H264/90000\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:203\r\n'
  + 'a=label:applicationsharing-video\r\na=mid:2\r\na=rtpmap:102 H264/90000\r\n'
  + 'm=x-data 3480 RTP/SAVP 127\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:204\r\na=label:data\r\na=mid:3\r\n'
  + 'a=x-data-protocol:sctp\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:205\r\na=label:main-video\r\na=mid:4\r\n'
  + 'a=sendonly\r\na=rtpmap:102 H264/90000\r\n'
  + 'm=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=x-source-streamid:206\r\na=label:main-video\r\na=mid:5\r\n'
  + 'a=sendonly\r\na=rtpmap:102 H264/90000\r\n';

describe('Teams group calls sending', () => {
  const caller: Caller = { mri: '8:orgid:me', name: 'Me', endpoint: 'ep', participant: 'p', surl: 'https://t/', random: 'r' };
  test('a group call tells what it sends on the links its acceptance names', () => {
    const accepted = { url: 'https://x/callAgent/ep/r/call/acceptance/', body: { callAcceptance: {
      links: { updateMediaDescriptions: 'https://fp/ep/cc/v1/active/1/updateMediaDescriptions?i=1',
        applyChannelParameters: 'https://fp/ep/cc/v1/mcProxy/1/applyChannelParameters?i=1' },
      mediaContent: { blob: 'v=0' } } } };
    const event = callPushEvent(accepted);
    assert.equal(event.kind, 'accepted');
    if (event.kind !== 'accepted') return;
    assert.equal(event.mediaDescriptions, 'https://fp/ep/cc/v1/active/1/updateMediaDescriptions?i=1');
    assert.equal(event.channelParameters, 'https://fp/ep/cc/v1/mcProxy/1/applyChannelParameters?i=1');
    const callback = callPushEvent({ body: { callAcceptance: { links: { updateMediaDescriptions: 'wss://t/x' }, mediaContent: { blob: 'v=0' } } } });
    assert.ok(callback.kind === 'accepted' && callback.mediaDescriptions === null);

    const descriptions = [{ mid: '1', direction: 'sendrecv', label: 'main-video' }, { mid: '2', direction: 'recvonly' }];
    assert.deepEqual(updateMediaDescriptionsBody(caller, descriptions, false, 2),
      { UpdateMediaDescriptions: { mediaDescriptions: { descriptions, negotiationTag: 'p;v_1', requestId: 2 } } });
    const share = updateMediaDescriptionsBody(caller, [], true, 3) as { UpdateMediaDescriptions: { mediaDescriptions: { negotiationTag: string } } };
    assert.equal(share.UpdateMediaDescriptions.mediaDescriptions.negotiationTag, 'p;ss_1');

    const invitation = describeMedia(startChannelCallBody(caller, '19:t@thread.tacv2', 'v=0'), [{ mid: '1', direction: 'recvonly' }]);
    const content = (invitation.callInvitation as { mediaContent: Record<string, unknown> }).mediaContent;
    assert.deepEqual(content.mediaDescriptions, { descriptions: [{ mid: '1', direction: 'recvonly' }], requestId: 1 });
    assert.equal(content.blob, 'v=0');

    const caps = cameraCapabilitiesBody('1', 1) as { applyChannelParameters: { multiChannelParameter: { mids: string[]; mediaParameter: string } } };
    assert.deepEqual(caps.applyChannelParameters.multiChannelParameter.mids, ['1']);
    assert.equal(caps.applyChannelParameters.multiChannelParameter.mediaParameter,
      '{"maxVideoSendCapabilities":{"caps":{"max-fps":30,"max-height":1280,"max-layers":1,"max-streams":1,"max-width":1280,"sequence-number":1}}}',
      'serde_json\'s sorted keys, as rv-teams writes it');

    const sharer = screenSharerBody(caller, 1, 'op') as { to: { id: string; propertyBag: unknown }[]; links: Record<string, string>; operationId: string };
    assert.equal(sharer.to[0]!.id, '8:orgid:me');
    assert.deepEqual(sharer.to[0]!.propertyBag, { screenSharerAttributes: { sequenceNumber: 1, value: { source: 1 } } });
    assert.equal(sharer.links.updateParticipantPropertiesStatus, 'https://t/callAgent/ep/r/conversation/updateParticipantPropertiesStatus/');
    assert.equal(sharer.operationId, 'op');
  });
  test('a group call offers its own sections up front and describes what they send', () => {
    const layout = Layout.group();
    const offer = teamsOffer(UP_FRONT_OFFER, NOT_SENDING, layout);
    const camera = section(offer, '1');
    for (const line of ['a=label:main-video', 'a=sendrecv', 'a=x-ssrc-range:51-51']) assert.ok(camera.includes(line), `${line}: ${camera}`);
    const screen = section(offer, '2');
    for (const line of ['a=label:applicationsharing-video', 'a=sendrecv', 'a=x-ssrc-range:61-61']) assert.ok(screen.includes(line), `${line}: ${screen}`);
    assert.ok(section(offer, '4').includes('a=recvonly') && section(offer, '5').includes('a=label:main-video'), offer);
    assert.equal(offer.split('a=label:applicationsharing-video').length - 1, 1, offer);
    assert.deepEqual(mediaDescriptions(layout, NOT_SENDING), [{ mid: '1', direction: 'recvonly' }, { mid: '2', direction: 'recvonly' }]);
    assert.deepEqual(mediaDescriptions(layout, { camera: true, screen: false }),
      [{ mid: '1', direction: 'sendrecv', label: 'main-video' }, { mid: '2', direction: 'recvonly' }]);
    assert.deepEqual(mediaDescriptions(layout, { camera: false, screen: true }),
      [{ mid: '1', direction: 'recvonly' }, { mid: '2', direction: 'sendonly', label: 'applicationsharing-video' }]);
    assert.deepEqual([ownMid(layout, 'camera'), ownMid(layout, 'screen')], ['1', '2']);

    assert.ok(plainAnswer(UP_FRONT_ANSWER, layout) !== null);
    assert.deepEqual(videoSlots(layout).map((s) => [s.streamId, s.label]),
      [[202, 'camera'], [203, 'screen'], [205, 'camera'], [206, 'camera']], 'this side\'s own sections receive too');
    const again = teamsOffer(UP_FRONT_OFFER, { camera: true, screen: true }, layout);
    assert.ok(section(again, '1').includes('a=sendrecv'), again);
    assert.ok(section(again, '2').includes('a=sendrecv'), again);
  });
  test('what this side sends is told one change at a time', () => {
    const off: Sending = NOT_SENDING;
    const camera: Sending = { camera: true, screen: false };
    const both: Sending = { camera: true, screen: true };
    assert.deepEqual(sendingSteps(off, off), []);
    assert.deepEqual(sendingSteps(off, camera), [[camera, 'camera']]);
    assert.deepEqual(sendingSteps(camera, { camera: false, screen: true }), [[off, 'camera'], [{ camera: false, screen: true }, 'screen']]);
    assert.deepEqual(sendingSteps(both, camera), [[camera, 'screen']]);
    assert.equal(ownMid(new Layout(), 'camera'), null);
  });
  test('a channel call offers its video up front and tells each change of it on its acceptance\'s links', async () => {
    let push: (envelope: Record<string, unknown>) => void = () => {};
    let tellSent: (now: Sending) => void = () => {};
    let id = 0;
    const started: unknown[] = [];
    const offers: unknown[] = [];
    const posted: [string, Record<string, unknown>][] = [];
    await joinThreadCall({
      post: async (url, _chain, body) => {
        posted.push([url, body]);
        return url.endsWith('/epconv') ? { links: { leave: 'https://conv/leave', updateParticipantProperties: 'https://conv/props' } } : {};
      },
      pushes: (listener) => { push = listener; return () => { push = () => {}; }; },
      poll: async () => ({}),
      put: async () => {},
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async (_identity, options) => { started.push(options); },
      offer: async () => UP_FRONT_OFFER,
      answer: async () => {},
      roster: async () => {},
      ended: async () => {},
      left: () => new Promise<void>(() => {}),
      offers: (listener) => { offers.push(listener); return () => {}; },
      sent: (listener) => { tellSent = listener; listener(NOT_SENDING); return () => { tellSent = () => {}; }; },
      sendData: async () => {},
    }, '19:c@thread.tacv2', null);
    assert.deepEqual(started, [{ dataChannel: 'main-channel', receiveVideos: 3, videoUpFront: true }]);
    const invitation = posted[0]![1].callInvitation as { mediaContent: { mediaDescriptions: unknown } };
    assert.deepEqual(invitation.mediaContent.mediaDescriptions,
      { descriptions: [{ mid: '1', direction: 'recvonly' }, { mid: '2', direction: 'recvonly' }], requestId: 1 });

    tellSent({ camera: true, screen: false });
    await settle();
    assert.equal(posted.length, 1, 'nothing is told before Teams took the call');
    const url = (path: string) => `https://t/callAgent/id1/id3/${path}`;
    push({ url: url('call/acceptance/'), body: { callAcceptance: {
      links: { updateMediaDescriptions: 'https://cc/describe', applyChannelParameters: 'https://cc/parameters' },
      mediaContent: { blob: UP_FRONT_ANSWER } } } });
    await settle();
    const told = () => posted.slice(1).filter(([u]) => u.startsWith('https://cc/') || u === 'https://conv/props');
    assert.deepEqual(told().map(([u]) => u), ['https://cc/parameters', 'https://cc/describe'], 'the camera\'s capabilities before it is first told');
    assert.deepEqual(told()[1]![1], { UpdateMediaDescriptions: { mediaDescriptions: {
      descriptions: [{ mid: '1', direction: 'sendrecv', label: 'main-video' }, { mid: '2', direction: 'recvonly' }], negotiationTag: 'id2;v_1', requestId: 2 } } });

    tellSent({ camera: true, screen: true });
    await settle();
    assert.deepEqual(told().slice(2).map(([u]) => u), ['https://conv/props', 'https://cc/describe'], 'the sharer before the share');
    const share = told()[3]![1] as { UpdateMediaDescriptions: { mediaDescriptions: { descriptions: unknown; negotiationTag: string; requestId: number } } };
    assert.deepEqual(share.UpdateMediaDescriptions.mediaDescriptions, {
      descriptions: [{ mid: '1', direction: 'sendrecv', label: 'main-video' }, { mid: '2', direction: 'sendonly', label: 'applicationsharing-video' }],
      negotiationTag: 'id2;ss_1', requestId: 3 });

    tellSent({ camera: false, screen: false });
    tellSent({ camera: true, screen: false });
    await settle();
    const later = told().slice(4);
    assert.deepEqual(later.map(([u]) => u), ['https://cc/describe', 'https://cc/describe', 'https://cc/describe'], 'the capabilities go once, the sharer only when a share starts');
    const steps = later.map(([, b]) => (b as { UpdateMediaDescriptions: { mediaDescriptions: { descriptions: { direction: string }[]; requestId: number } } }).UpdateMediaDescriptions.mediaDescriptions);
    assert.deepEqual(steps.map((m) => [m.requestId, m.descriptions.map((d) => d.direction).join(' ')]),
      [[4, 'recvonly sendonly'], [5, 'recvonly recvonly'], [6, 'sendrecv recvonly']]);
    assert.equal(offers.length, 1);
    push({ url: url('conversation/end/'), body: { conversationEnd: {} } });
    await settle();
  });
});

describe('teams thread calls', () => {
  test('a thread joins its running call, else its meeting', () => {
    const live = JSON.stringify({ status: 'Active', groupCallInitiator: '8:orgid:starter', meetingInfo: { organizerId: 'org', tenantId: 'ten' } });
    const meeting = JSON.stringify({ organizerId: 'planner', tenantId: 'ten2', meetingJoinUrl: 'https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%7d' });
    assert.deepEqual(meetingJoin({ properties: { 'awareness_conversationLiveState:0': live, meeting } }, '19:t@thread.v2', 'mine'),
      { thread: '19:t@thread.v2', message: '0', organizer: 'org', tenant: 'ten' });
    const planned = { threadProperties: { meeting, 'awareness_conversationLiveState:0': JSON.stringify({ status: 'Ended' }) } };
    assert.deepEqual(meetingJoin(planned, '19:meeting_x@thread.v2', 'mine'), { thread: '19:meeting_x@thread.v2', message: '0', organizer: 'planner', tenant: 'ten2' });
    const huddle = { properties: { 'awareness_conversationLiveState:0': JSON.stringify({ status: 'Active', groupCallInitiator: '8:orgid:starter' }) } };
    assert.deepEqual(meetingJoin(huddle, '19:c@thread.tacv2', 'mine'), { thread: '19:c@thread.tacv2', message: '0', organizer: 'starter', tenant: 'mine' });
    assert.equal(meetingJoin({ properties: { topic: 'x' } }, '19:c@thread.tacv2', 'mine'), null);
    assert.equal(joinMessage('https://teams.microsoft.com/l/meetup-join/19%3ab%40thread.tacv2/1700000000000?context=x'), '1700000000000');
    assert.equal(joinMessage('https://teams.microsoft.com/l/call/0/0?users=a'), null);
  });
  const run = async (join: Parameters<typeof joinThreadCall>[3]) => {
    const posted: [string, Record<string, unknown>][] = [];
    let id = 0;
    await joinThreadCall({
      post: async (url, _chain, body) => {
        posted.push([url, body]);
        return url.endsWith('/epconv') && join !== null ? { conversationController: 'https://conv/c1' } : { links: { leave: 'https://conv/leave' } };
      },
      pushes: () => () => {},
      poll: async () => ({}),
      put: async () => {},
      id: () => `id${++id}`,
      me: { mri: '8:orgid:me', name: 'Me' },
      surl: 'https://t/',
    }, {
      start: async () => {}, offer: async () => OFFER, answer: async () => {}, roster: async () => {},
      ended: async () => {}, left: () => new Promise<void>(() => {}),
    }, '19:c@thread.tacv2', join);
    return posted;
  };
  test('a channel with no call starts its own, ringing no one', async () => {
    const posted = await run(null);
    assert.equal(posted.length, 1);
    const [url, body] = posted[0]!;
    assert.ok(url.endsWith('/epconv'));
    assert.equal((body.conversationRequest as Record<string, unknown>).conversationType, 'huddleGroupCall');
    assert.deepEqual(body.groupChat, { threadId: '19:c@thread.tacv2', messageId: null });
    assert.deepEqual((body.participants as Record<string, unknown>).to, []);
  });
  test('a meeting is resolved, then its call joined on its controller', async () => {
    const posted = await run({ thread: '19:m@thread.v2', message: '0', organizer: 'org', tenant: 'ten' });
    assert.deepEqual(posted.map(([url]) => url), ['https://api.flightproxy.teams.microsoft.com/api/v2/epconv', 'https://conv/c1']);
    const [[, resolve], [, call]] = posted as [[string, Record<string, unknown>], [string, Record<string, unknown>]];
    assert.deepEqual(resolve.meetingInfo, { tenantId: 'ten', organizerId: 'org' });
    assert.equal(resolve.callInvitation, undefined);
    assert.deepEqual(call.groupChat, { threadId: '19:m@thread.v2', messageId: '0' });
    assert.equal(((call.callInvitation as Record<string, unknown>).links as Record<string, unknown>).acceptance, 'https://t/callAgent/id1/id3/call/acceptance/');
  });
});

