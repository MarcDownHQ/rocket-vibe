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

export type RosterChange = { mri: string; name: string; present: boolean };
export type CallPushEvent =
  | { kind: 'accepted'; sdp: string; acknowledge: string | null }
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
    const link = (accepted?.links as Doc | undefined)?.acknowledgement;
    return { kind: 'accepted', sdp: blob, acknowledge: typeof link === 'string' ? link : null };
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

export function leaveBody(c: Caller): Doc {
  return {
    participants: { from: fromOf(c) },
    conversationTransactionEnd: { reason: 'noError', code: 0, phrase: 'ConversationEndNoModalityConnected' },
  };
}

type CallDeps = {
  post: (url: string, chain: string, body: Doc) => Promise<Doc>;
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
  const queued: Doc[] = [];
  let handle: (envelope: Doc) => void = (envelope) => { queued.push(envelope); };
  const unsubscribe = deps.pushes((envelope) => handle(envelope));
  let created: Doc;
  try {
    created = await deps.post(EPCONV, chain, placeCallBody(caller, to, thread, teamsOffer(offer), deps.id()));
  } catch (error) {
    unsubscribe();
    throw error;
  }
  const leaveLink = (created.links as Doc | undefined)?.leave;
  const leave = () => { if (typeof leaveLink === 'string') void deps.post(leaveLink, chain, leaveBody(caller)).catch(() => {}); };
  let done = false;
  let answered = false;
  const finish = (reason: string | null) => {
    if (done) return;
    done = true;
    clearTimeout(ringing);
    unsubscribe();
    if (reason !== null) void engine.ended(reason);
  };
  const ringing = setTimeout(() => { if (!answered) { finish('no_answer'); leave(); } }, RINGING_MS);
  const roster = new Map<string, string>();
  handle = (envelope) => {
    if (done || !isCallCallback(envelope, caller.endpoint)) return;
    const event = callPushEvent(envelope);
    if (event.kind === 'accepted' && !answered) {
      const answer = webrtcAnswer(event.sdp);
      if (answer === null) { finish('connect_failed'); leave(); return; }
      answered = true;
      void engine.answer(answer);
      if (event.acknowledge !== null) void deps.post(event.acknowledge, chain, acknowledgeBody(caller)).catch(() => {});
    } else if (event.kind === 'roster') {
      for (const change of event.changes) {
        if (change.mri.toLowerCase() === caller.mri.toLowerCase()) continue;
        if (change.present) roster.set(change.mri, change.name);
        else roster.delete(change.mri);
      }
      void engine.roster([...roster].map(([id, name]) => ({ id, name, muted: false })));
    } else if (event.kind === 'ended') finish('remote_ended');
  };
  void engine.left().then(() => { if (!done) { finish(null); leave(); } });
  for (const envelope of queued) handle(envelope);
}
