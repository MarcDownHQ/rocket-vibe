/**
 * Teams' calling stack, carried by the voice engine: the app speaks the
 * signalling over the account's tokens and socket, the engine offers and carries
 * the media. Its SDP dialect against plain WebRTC's, what its pushes carry, the
 * bodies it wants, and the call's run. Mirrors `calling.rs` in crates/rv-teams and
 * `teams_call.rs` in rv-core; brain/architecture/native-calls.md.
 */

import type { ExternalCallEngine } from '@rocket-vibe/provider-api/calls';

type Doc = Record<string, unknown>;

export const ECHO_BOT = 'cf28171e-fcfd-47e4-a1d6-79460b0b3ca0';
export const EPCONV = 'https://api.flightproxy.teams.microsoft.com/api/v2/epconv';
export const CALL_CLIENT = 'SkypeSpaces/1415/26100212018/os=linux; osVer=undefined; deviceType=computer; browser=chrome';
const RINGING_MS = 90_000;

type OfferCandidate = { udp: boolean; address: string; port: string; plain: string };

function candidateFields(line: string): OfferCandidate | null {
  if (!line.startsWith('a=candidate:')) return null;
  const fields = line.slice('a=candidate:'.length).split(' ');
  if (fields.length < 6) return null;
  const end = fields.indexOf('generation');
  return {
    udp: fields[2]!.toLowerCase() === 'udp',
    address: fields[4]!,
    port: fields[5]!,
    plain: (end < 0 ? fields : fields.slice(0, end)).join(' '),
  };
}

/**
 * A plain WebRTC offer, audio only, in Teams' dialect. The media line names a real
 * address from a candidate: port 9 and `0.0.0.0` read as disabled media to Teams.
 */
export function teamsOffer(sdp: string): string {
  const lines = sdp.split(/\r?\n/).filter((l) => l !== '');
  const ssrc = lines.find((l) => l.startsWith('a=ssrc:'))?.slice('a=ssrc:'.length).split(' ')[0] ?? null;
  const chosen = lines.map(candidateFields).find((c): c is OfferCandidate => c !== null && c.udp && !c.address.includes(':'));
  const port = chosen?.port ?? '9';
  const address = chosen?.address ?? '0.0.0.0';
  const out: string[] = [];
  for (const line of lines) {
    if (line.startsWith('m=audio ')) {
      const rest = line.slice('m=audio '.length).split(' ').slice(1).join(' ');
      out.push(`m=audio ${port} ${rest.replace('UDP/TLS/RTP/SAVPF', 'RTP/SAVP')}`);
    } else if (line.startsWith('c=')) out.push(`c=IN IP4 ${address}`);
    else if (line.startsWith('a=rtcp:')) out.push(`a=rtcp:${port}`);
    else if (line.startsWith('a=extmap:')) out.push(line.replaceAll('/', '\\'));
    else if (line === 'a=rtcp-rsize') continue;
    else if (line.startsWith('a=candidate:')) {
      const candidate = candidateFields(line);
      if (candidate?.udp) out.push(`a=candidate:${candidate.plain}`);
    } else if (line.startsWith('a=mid:')) {
      out.push(line, 'a=label:main-audio', 'a=x-signaling-fb:* x-message app recv:dsh');
      if (ssrc !== null) out.push(`a=x-ssrc-range:${ssrc}-${ssrc}`);
    } else out.push(line);
  }
  return `${out.join('\r\n')}\r\n`;
}

/** Teams' answer in plain WebRTC: only its audio section, what libwebrtc reads. */
export function webrtcAnswer(sdp: string): string | null {
  const head: string[] = [];
  let audio: string[] | null = null;
  let at: 'head' | 'audio' | 'other' = 'head';
  for (const line of sdp.split(/\r?\n/)) {
    if (line === '') continue;
    if (line.startsWith('m=')) {
      if (line.startsWith('m=audio ') && audio === null) {
        audio = [line.replace('RTP/SAVP', 'UDP/TLS/RTP/SAVPF')];
        at = 'audio';
      } else at = 'other';
      continue;
    }
    if (at === 'head') {
      if (!line.startsWith('a=group:') && !line.startsWith('a=x-')) head.push(line);
      continue;
    }
    if (at === 'other' || audio === null) continue;
    if (line.startsWith('a=x-') || line.startsWith('a=label:') || line.startsWith('a=ptime:')) continue;
    if (line.startsWith('a=candidate:')) {
      const fields = line.slice('a=candidate:'.length).split(' ');
      if (fields[2]?.toLowerCase() !== 'udp') continue;
      const end = fields.indexOf('MTURNID');
      audio.push(`a=candidate:${(end < 0 ? fields : fields.slice(0, end)).join(' ')}`);
    } else if (line.startsWith('a=extmap:')) audio.push(line.replaceAll('\\', '/'));
    else audio.push(line);
  }
  if (audio === null) return null;
  const mid = audio.find((l) => l.startsWith('a=mid:'))?.slice('a=mid:'.length);
  if (mid === undefined) return null;
  const out: string[] = [];
  for (const line of head) {
    out.push(line);
    if (line.startsWith('t=')) out.push(`a=group:BUNDLE ${mid}`);
  }
  return `${[...out, ...audio].join('\r\n')}\r\n`;
}

const sdpLines = (sdp: string) => sdp.split(/\r?\n/).filter((l) => l !== '');

/** The first audio section's `a=mid`. */
export function audioMid(sdp: string): string | null {
  const lines = sdpLines(sdp);
  const start = lines.findIndex((l) => l.startsWith('m=audio '));
  if (start < 0) return null;
  return lines.slice(start).find((l) => l.startsWith('a=mid:'))?.slice('a=mid:'.length) ?? null;
}

function withMid(sdp: string, mid: string): string {
  const out = sdpLines(sdp).map((l) => (l.startsWith('a=mid:') ? `a=mid:${mid}` : l.startsWith('a=group:BUNDLE') ? `a=group:BUNDLE ${mid}` : l));
  return `${out.join('\r\n')}\r\n`;
}

/** A new offer of Teams' in plain WebRTC, under the `mid` the call already negotiated:
 *  libwebrtc refuses a later offer whose section has another one. */
export function renegotiationOffer(sdp: string, sessionMid: string): string | null {
  const plain = webrtcAnswer(sdp);
  return plain === null ? null : withMid(plain, sessionMid);
}

/** A local answer to Teams' `offer`, in Teams' dialect: the offer's sections in its
 *  order, each but the audio one refused with port 0. */
export function teamsAnswer(answer: string, offer: string): string {
  const mid = audioMid(offer);
  const audio = teamsOffer(mid === null ? answer : withMid(answer, mid));
  const at = audio.indexOf('m=audio ');
  const head = at < 0 ? audio : audio.slice(0, at);
  const body = at < 0 ? '' : audio.slice(at);
  const sections: string[][] = [];
  for (const line of sdpLines(offer)) {
    if (line.startsWith('m=')) sections.push([line]);
    else sections.at(-1)?.push(line);
  }
  let out = head;
  let placed = false;
  for (const section of sections) {
    const line = section[0]!;
    if (line.startsWith('m=audio ') && !placed) { placed = true; out += body; continue; }
    const [kind, , ...rest] = line.split(' ');
    out += `${kind} 0 ${rest.join(' ')}\r\n`;
    const sectionMid = section.find((l) => l.startsWith('a=mid:'));
    if (sectionMid !== undefined) out += `${sectionMid}\r\n`;
  }
  return out;
}

export type RosterChange = { mri: string; name: string; present: boolean };
export type CallPushEvent =
  | { kind: 'accepted'; sdp: string; acknowledge: string | null; callLeg: string | null }
  | { kind: 'negotiation'; sdp: string; answerLink: string; mediaLeg: string | null }
  | { kind: 'ended' }
  | { kind: 'roster'; changes: RosterChange[] }
  | { kind: 'other' };

function bodyOf(envelope: Doc): Doc | null {
  let body = envelope.body;
  if (typeof body === 'string') {
    try { body = JSON.parse(body) as unknown; } catch { return null; }
  }
  return typeof body === 'object' && body !== null ? (body as Doc) : null;
}

export function isCallCallback(envelope: Doc, endpoint: string): boolean {
  return typeof envelope.url === 'string' && envelope.url.includes(`callAgent/${endpoint}/`);
}

export function callPushEvent(envelope: Doc): CallPushEvent {
  const body = bodyOf(envelope);
  if (body === null) return { kind: 'other' };
  const accepted = (body.callAcceptance ?? body.mediaAnswer) as Doc | undefined;
  const blob = (accepted?.mediaContent as Doc | undefined)?.blob;
  if (typeof blob === 'string') {
    const links = accepted?.links as Doc | undefined;
    const text = (v: unknown) => (typeof v === 'string' ? v : null);
    return { kind: 'accepted', sdp: blob, acknowledge: text(links?.acknowledgement), callLeg: text(links?.callLeg) };
  }
  const negotiation = body.mediaNegotiation as Doc | undefined;
  const offered = (negotiation?.mediaContent as Doc | undefined)?.blob;
  const answerLink = (negotiation?.links as Doc | undefined)?.mediaAnswer;
  if (typeof offered === 'string' && typeof answerLink === 'string') {
    const leg = (negotiation?.mediaContent as Doc | undefined)?.mediaLegId;
    return { kind: 'negotiation', sdp: offered, answerLink, mediaLeg: typeof leg === 'string' ? leg : null };
  }
  if (typeof envelope.url === 'string' && envelope.url.includes('/rosterUpdate')
    && typeof body.participants === 'object' && body.participants !== null) {
    const changes = Object.entries(body.participants as Record<string, Doc>).map(([mri, entry]) => {
      const name = (entry.details as Doc | undefined)?.displayName;
      const endpoints = entry.endpoints;
      return {
        mri,
        name: typeof name === 'string' ? name : '',
        present: typeof endpoints === 'object' && endpoints !== null && Object.keys(endpoints).length > 0,
      };
    });
    return { kind: 'roster', changes };
  }
  if (body.callEnd !== undefined || body.conversationEnd !== undefined || body.sessionRejection !== undefined) return { kind: 'ended' };
  return { kind: 'other' };
}

export type Caller = { mri: string; name: string; endpoint: string; participant: string; surl: string; random: string };

const callback = (c: Caller, path: string) => `${c.surl}callAgent/${c.endpoint}/${c.random}/${path}`;
const fromOf = (c: Caller) => ({ id: c.mri, displayName: c.name, endpointId: c.endpoint, participantId: c.participant, languageId: 'en-us' });

/** `epconv`'s body placing a call to `to`, a group's with its `thread`. */
export function placeCallBody(c: Caller, to: readonly string[], thread: string | null, offer: string, target: string): Doc {
  const cb = (path: string) => callback(c, path);
  return {
    conversationRequest: {
      conversationType: null, subject: null, suppressDialout: false,
      roster: { type: 'Delta', rosterUpdate: cb('conversation/rosterUpdate/') },
      properties: { allowConversationWithoutHost: true, enableGroupCallEventMessages: true, enableGroupCallUpgradeMessage: false, enableGroupCallMeetupGeneration: false },
      links: {
        conversationEnd: cb('conversation/conversationEnd/'),
        conversationUpdate: cb('conversation/conversationUpdate/'),
        localParticipantUpdate: cb('conversation/localParticipantUpdate/'),
        addParticipantSuccess: cb('conversation/addParticipantSuccess/'),
        addParticipantFailure: cb('conversation/addParticipantFailure/'),
        addModalitySuccess: cb('conversation/addModalitySuccess/'),
        addModalityFailure: cb('conversation/addModalityFailure/'),
        confirmUnmute: cb('conversation/confirmUnmute/'),
        receiveMessage: cb('conversation/receiveMessage/'),
      },
    },
    groupContext: null,
    groupChat: thread === null ? null : { threadId: thread, messageId: null },
    participants: { from: fromOf(c), to: to.map((id) => ({ id, participantId: target })) },
    capabilities: null,
    endpointCapabilities: 73463,
    clientEndpointCapabilities: 114259690,
    endpointMetadata: { holographicCapabilities: 3 },
    meetingInfo: null,
    endpointState: { endpointStateSequenceNumber: 1, endpointProperties: { additionalEndpointProperties: { infoShownInReportMode: 'FullInformation' } } },
    callInvitation: {
      callModalities: ['Audio'], replaces: null, transferor: null,
      links: { progress: cb('call/progress/'), mediaAnswer: cb('call/mediaAnswer/'), acceptance: cb('call/acceptance/'), redirection: cb('call/redirection/'), end: cb('call/end/') },
      clientContentForMediaController: { controlVideoStreaming: cb('call/controlVideoStreaming/'), csrcInfo: cb('call/csrcInfo/') },
      pstnContent: { emergencyCallCountry: '', platformName: CALL_CLIENT, publicApiCall: false },
      mediaContent: { blob: offer, contentType: 'application/sdp-ngc-1.0', requiredFeatures: 'nonByPass' },
    },
  };
}

/** The acknowledgement a `callAcceptance` waits for: without it the call ends (430). */
export function acknowledgeBody(c: Caller): Doc {
  const cb = (path: string) => callback(c, path);
  const names = ['acknowledgement', 'mediaRenegotiation', 'replacement', 'progress', 'mediaAnswer', 'newMediaOffer', 'redirection', 'balanceUpdate', 'acceptance', 'controlVideoStreaming', 'csrcInfo', 'end', 'retargetCompletion', 'transfer', 'transferAcceptance', 'transferCompletion', 'holdCompletion', 'resumeCompletion'];
  return { callAcceptanceAcknowledgement: { links: Object.fromEntries(names.map((name) => [name, cb(`call/${name}/`)])) } };
}

/** The call leg's registration, as Teams' web client sends it once accepted. */
export function registerOnLegBody(c: Caller): Doc {
  const ack = acknowledgeBody(c);
  return { ...ack, callParticipantUpdate: ack.callAcceptanceAcknowledgement };
}

export function endpointMetadataBody(c: Caller): Doc {
  return { participants: { from: fromOf(c) }, endpointMetadata: { holographicCapabilities: 3 } };
}

/** `mediaAnswer`'s body answering a renegotiation. */
export function answerNegotiationBody(answer: string, mediaLeg: string | null): Doc {
  return { mediaAnswer: { callModalities: ['Audio'], mediaContent: { blob: answer, contentType: 'application/sdp-ngc-1.0', mediaLegId: mediaLeg, escalationOccurring: false, newOffer: false } } };
}

/** The broker's next long poll: its lease is 30 s, and a call whose lease lapses dies. */
export function nextSubscription(answer: Doc): string | null {
  const next = answer.nextSubscribeUrl;
  return typeof next === 'string' && next.startsWith('https://') ? next : null;
}

export function leaveBody(c: Caller): Doc {
  return {
    participants: { from: fromOf(c) },
    conversationTransactionEnd: { reason: 'noError', code: 0, phrase: 'ConversationEndNoModalityConnected' },
  };
}

type CallDeps = {
  post: (url: string, chain: string, body: Doc) => Promise<Doc>;
  /** A long poll (the conversation's broker). */
  poll: (url: string, chain: string) => Promise<Doc>;
  put: (url: string, chain: string, body: Doc) => Promise<void>;
  sleep?: (ms: number) => Promise<void>;
  pushes: (listener: (envelope: Doc) => void) => () => void;
  id: () => string;
  me: { mri: string; name: string };
  surl: string;
};

/** Places a call on `engine`; resolves once Teams took it, the call then running until it ends or is left. */
export async function placeExternalCall(deps: CallDeps, engine: ExternalCallEngine, to: readonly string[], thread: string | null): Promise<void> {
  const caller: Caller = { ...deps.me, endpoint: deps.id(), participant: deps.id(), surl: deps.surl, random: deps.id() };
  const chain = deps.id();
  await engine.start(caller.endpoint);
  const offer = await engine.offer();
  const pushes = listen(deps);
  let created: Doc;
  try {
    created = await deps.post(EPCONV, chain, placeCallBody(caller, to, thread, teamsOffer(offer), deps.id()));
  } catch (error) {
    pushes.stop();
    throw error;
  }
  runCall(deps, engine, caller, chain, (created.links ?? {}) as Doc, pushes, null);
}

/** A ringing call's offer and the links that answer it (`callNotification` of its push). */
export type TeamsAnswering = { attach: string; offer: string; mediaLeg: string | null; conversation: string };

/** `attach`'s body, answering a ring here: this endpoint's callbacks, and the join of its conversation. */
export function attachBody(c: Caller, conversation: string): Doc {
  const cb = (path: string) => callback(c, path);
  return {
    attach: { requireMediaContent: false, links: { end: cb('call/end/') }, locationContent: null, networkContent: null, areaContent: null },
    capabilities: null,
    endpointCapabilities: 73463,
    additionalActions: [{
      name: 'join',
      url: conversation,
      waitForResponse: true,
      input: {
        capabilities: null,
        endpointCapabilities: 73463,
        conversationRequest: {
          roster: { type: 'Delta', rosterUpdate: cb('conversation/rosterUpdate/') },
          links: {
            conversationEnd: cb('conversation/conversationEnd/'),
            conversationUpdate: cb('conversation/conversationUpdate/'),
            localParticipantUpdate: cb('conversation/localParticipantUpdate/'),
            addParticipantSuccess: cb('conversation/addParticipantSuccess/'),
            addParticipantFailure: cb('conversation/addParticipantFailure/'),
            receiveMessage: cb('conversation/receiveMessage/'),
          },
        },
        endpointMetadata: {},
        participants: { from: fromOf(c) },
      },
    }],
  };
}

/** An `attach`'s answer as one set of links, the conversation's first (`leave`), then the call's (`acceptance`, `subscribe`). */
export function attachedLinks(answer: Doc): Doc | null {
  const invitation = (answer.callInvitation as Doc | undefined)?.links as Doc | undefined;
  const responses = answer.additionalActionResponses;
  const joined = Array.isArray(responses) ? ((responses[0] as Doc | undefined)?.output as Doc | undefined) : undefined;
  if (invitation === undefined || joined === undefined) return null;
  return { ...invitation, ...((joined.links ?? {}) as Doc) };
}

/** `acceptance`'s body: the answer in Teams' dialect, under the offer's media leg. */
export function acceptBody(c: Caller, answer: string, mediaLeg: string | null): Doc {
  const cb = (path: string) => callback(c, path);
  return {
    callAcceptance: {
      acceptedBy: fromOf(c),
      acceptedCallModalities: ['Audio'],
      capabilities: null,
      endpointCapabilities: 73463,
      clientEndpointCapabilities: 114259690,
      links: {
        mediaRenegotiation: cb('call/mediaRenegotiation/'), transfer: cb('call/transfer/'), replacement: cb('call/replacement/'),
        balanceUpdate: cb('call/balanceUpdate/'), retargetCompletion: cb('call/retargetCompletion/'),
        controlVideoStreaming: cb('call/controlVideoStreaming/'), updateMediaDescriptions: cb('call/updateMediaDescriptions'),
      },
      clientContentForMediaController: { controlVideoStreaming: cb('call/controlVideoStreaming/'), csrcInfo: cb('call/csrcInfo/') },
      mediaContent: { blob: answer, contentType: 'application/sdp-ngc-1.0', mediaLegId: mediaLeg },
      pstnContent: { emergencyCallCountry: '', platformName: CALL_CLIENT, publicApiCall: false },
      callKeepAliveInterval: null,
    },
  };
}

/** Answers a ringing call on `engine`: attaches, answers its offer and accepts; then the call runs as a placed one. */
export async function answerExternalCall(deps: CallDeps, engine: ExternalCallEngine, ring: TeamsAnswering): Promise<void> {
  if (engine.answerOffer === undefined) throw new Error('call_not_answerable');
  const caller: Caller = { ...deps.me, endpoint: deps.id(), participant: deps.id(), surl: deps.surl, random: deps.id() };
  const chain = deps.id();
  const plain = webrtcAnswer(ring.offer);
  if (plain === null) throw new Error('call_offer_invalid');
  const pushes = listen(deps);
  try {
    const links = attachedLinks(await deps.post(ring.attach, chain, attachBody(caller, ring.conversation)));
    if (links === null || typeof links.acceptance !== 'string') throw new Error('call_attach_failed');
    await engine.start(caller.endpoint);
    const local = await engine.answerOffer(plain);
    if (local === null) throw new Error('call_answer_failed');
    await deps.post(links.acceptance, chain, acceptBody(caller, teamsAnswer(local, ring.offer), ring.mediaLeg));
    runCall(deps, engine, caller, chain, links, pushes, audioMid(plain) ?? '0');
  } catch (error) {
    pushes.stop();
    throw error;
  }
}

type Pushes = { stop: () => void; attach: (handle: (envelope: Doc) => void) => void };

/** The call's pushes, held from before its first request until the run takes them. */
function listen(deps: CallDeps): Pushes {
  const queued: Doc[] = [];
  let handle: (envelope: Doc) => void = (envelope) => { queued.push(envelope); };
  const stop = deps.pushes((envelope) => handle(envelope));
  return {
    stop,
    attach: (next) => {
      handle = next;
      for (const envelope of queued.splice(0)) next(envelope);
    },
  };
}

/**
 * The call's run until it ends: the broker lease, the endpoint metadata, the
 * answer (when placed here), renegotiations, the roster and the end. `sessionMid`
 * is the audio section's mid of a call answered here, accepted already.
 */
function runCall(deps: CallDeps, engine: ExternalCallEngine, caller: Caller, chain: string, links: Doc, pushes: Pushes, accepted: string | null): void {
  const leaveLink = links.leave;
  const leave = () => { if (typeof leaveLink === 'string') void deps.post(leaveLink, chain, leaveBody(caller)).catch(() => {}); };
  let done = false;
  let answered = accepted !== null;
  let sessionMid: string | null = accepted;
  const sleep = deps.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  const holdLease = async (first: string) => {
    let url = first;
    let failures = 0;
    while (!done) {
      try {
        url = nextSubscription(await deps.poll(url, chain)) ?? url;
        failures = 0;
      } catch {
        if (++failures > 5) return;
        await sleep(2000);
      }
    }
  };
  if (typeof links.subscribe === 'string') void holdLease(links.subscribe);
  if (typeof links.updateEndpointMetadata === 'string') void deps.put(links.updateEndpointMetadata, chain, endpointMetadataBody(caller)).catch(() => {});
  const finish = (reason: string | null) => {
    if (done) return;
    done = true;
    clearTimeout(ringing);
    pushes.stop();
    if (reason !== null) void engine.ended(reason);
  };
  const ringing = setTimeout(() => { if (!answered) { finish('no_answer'); leave(); } }, RINGING_MS);
  const roster = new Map<string, string>();
  pushes.attach((envelope) => {
    if (done || !isCallCallback(envelope, caller.endpoint)) return;
    const event = callPushEvent(envelope);
    if (event.kind === 'accepted' && !answered) {
      const answer = webrtcAnswer(event.sdp);
      if (answer === null) { finish('connect_failed'); leave(); return; }
      answered = true;
      sessionMid = audioMid(answer);
      void engine.answer(answer);
      if (event.acknowledge !== null) void deps.post(event.acknowledge, chain, acknowledgeBody(caller)).catch(() => {});
      if (event.callLeg !== null) void deps.post(event.callLeg, chain, registerOnLegBody(caller)).catch(() => {});
    } else if (event.kind === 'negotiation' && engine.answerOffer !== undefined) {
      const plain = renegotiationOffer(event.sdp, sessionMid ?? audioMid(event.sdp) ?? '0');
      if (plain === null) return;
      void engine.answerOffer(plain).then((local) => {
        if (local === null || done) return;
        return deps.post(event.answerLink, chain, answerNegotiationBody(teamsAnswer(local, event.sdp), event.mediaLeg));
      }).catch(() => {});
    } else if (event.kind === 'roster') {
      for (const change of event.changes) {
        if (change.mri.toLowerCase() === caller.mri.toLowerCase()) continue;
        if (change.present) roster.set(change.mri, change.name);
        else roster.delete(change.mri);
      }
      void engine.roster([...roster].map(([id, name]) => ({ id, name, muted: false })));
    } else if (event.kind === 'ended') finish('remote_ended');
  });
  void engine.left().then(() => { if (!done) { finish(null); leave(); } });
}
