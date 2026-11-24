/**
 * Teams' calling stack, carried by the voice engine: the app speaks the
 * signalling over the account's tokens and socket, the engine offers and carries
 * the media. Its SDP dialect against plain WebRTC's, what its pushes carry, the
 * bodies it wants, and the call's run. Mirrors `calling.rs` and `sdp.rs` in crates/rv-teams and
 * `teams_call.rs` in rv-core; brain/architecture/native-calls.md.
 */

import type { ExternalCallEngine } from '@rocket-vibe/provider-api/calls';
import { Channel, LABEL as DATA_LABEL, Routing, type Source, type VideoSlot } from './datachannel.ts';

export type { Source, VideoSlot } from './datachannel.ts';

type Doc = Record<string, unknown>;

export const ECHO_BOT = 'cf28171e-fcfd-47e4-a1d6-79460b0b3ca0';
export const EPCONV = 'https://api.flightproxy.teams.microsoft.com/api/v2/epconv';
export const CALL_CLIENT = 'SkypeSpaces/1415/26100212018/os=linux; osVer=undefined; deviceType=computer; browser=chrome';
const RINGING_MS = 90_000;
/** The receiving video sections a group call offers: the last for a shared screen, the others for cameras (this side's own on the first). */
const GROUP_VIDEOS = 5;

/**
 * Teams' SDP dialect against plain WebRTC's (`sdp.rs` in crates/rv-teams). Teams
 * names a section by its `a=label` (`main-audio`, `main-video` for the cameras,
 * `applicationsharing-video` for a shared screen) and renumbers the `a=mid`s from
 * one offer to the next, while libwebrtc keeps every section it negotiated in its
 * place under its own mid. A `Layout` holds a call's plain sections across its
 * negotiations: the audio stays in its section whatever mid Teams gives it, a
 * video section under a mid the call has not seen becomes a section of its own,
 * and a section Teams no longer lists is refused. On Teams' side the media line
 * names a real address from a candidate: port 9 and `0.0.0.0` read as disabled media.
 * Its `m=x-data` section is a plain SCTP data channel under another name: a group
 * call's media server takes its requests on it (datachannel.ts), and names each
 * section's stream in its answer (`a=x-source-streamid`).
 */

/** What a Teams section carries. */
export type Label = 'audio' | 'camera' | 'screen';

const LABELS: Record<Label, string> = { audio: 'main-audio', camera: 'main-video', screen: 'applicationsharing-video' };

const labelOf = (name: string): Label | null => (Object.keys(LABELS) as Label[]).find((l) => LABELS[l] === name) ?? null;

/** What this side sends now: a section of its own camera or screen that sends nothing is told to Teams as not sending. */
export type Sending = { camera: boolean; screen: boolean };

export const NOT_SENDING: Sending = { camera: false, screen: false };

/** The stream id the voice engine gives this side's camera and screen tracks. */
const CAMERA_STREAM = '-camera';
const SCREEN_STREAM = '-screen';

/** A call's sections across its negotiations. */
export class Layout {
  /** The plain SDP libwebrtc last took from the other side, offer or answer: its sections are the call's, in their order. */
  remote = '';
  /** This side's plain offer, waiting for Teams' answer. */
  offered: string | null = null;
  /** What each plain video section carries, by its mid. */
  labels = new Map<string, Label>();
  /** The plain mid of each section of Teams' last offer, by Teams' mid. */
  mids = new Map<string, string>();
  /** The stream id Teams gives each plain section, by its mid. */
  streams = new Map<string, number>();
  /** A group call's: every receiving video section is offered, the last one nothing labelled yet as the shared screen's. */
  group = false;

  /** A group call's layout. */
  static group(): Layout {
    const layout = new Layout();
    layout.group = true;
    return layout;
  }

  /** Whether an offer of this side's waits for Teams' answer. */
  offering(): boolean {
    return this.offered !== null;
  }

  /** Forgets this side's offer: Teams offered meanwhile, and its answer, should it come, answers an offer the engine rolled back. */
  withdraw(): void {
    this.offered = null;
  }
}

type Section = readonly string[];

const DIRECTIONS = ['sendrecv', 'sendonly', 'recvonly', 'inactive'];

function sdpLines(sdp: string): string[] {
  const out = sdp.split('\n').map((l) => (l.endsWith('\r') ? l.slice(0, -1) : l));
  if (out.at(-1) === '') out.pop();
  return out;
}

const field = (s: Section, at: number) => (s[0]!.startsWith('m=') ? s[0]!.slice(2) : '').split(' ')[at] ?? '';
const kindOf = (s: Section) => field(s, 0);
const isOpen = (s: Section) => field(s, 1) !== '0';
/** The payload types, after the profile. */
const formats = (s: Section) => s[0]!.split(' ').slice(3).join(' ');
const after = (s: Section, prefix: string) => s.find((l) => l.startsWith(prefix))?.slice(prefix.length) ?? null;
const midOf = (s: Section) => after(s, 'a=mid:');
const isDirection = (line: string) => DIRECTIONS.some((d) => line === `a=${d}`);
const directionOf = (s: Section) => s.find(isDirection)?.slice(2) ?? 'sendrecv';
const labelIn = (s: Section) => s.filter((l) => l.startsWith('a=label:')).map((l) => labelOf(l.slice('a=label:'.length))).find((l) => l !== null) ?? null;
const streamOf = (s: Section) => after(s, 'a=msid:')?.split(' ')[0] ?? null;
const ssrcOf = (s: Section) => after(s, 'a=ssrc:')?.split(' ')[0] ?? null;
/** The stream id Teams gives the section. */
function streamIdOf(s: Section): number | null {
  const value = after(s, 'a=x-source-streamid:')?.trim() ?? null;
  if (value === null || !/^\+?\d+$/.test(value)) return null;
  const id = Number(value);
  return id <= 0xffffffff ? id : null;
}

/** This side's own camera or screen, by the stream id the voice engine gives it. */
function own(s: Section): Label | null {
  const stream = streamOf(s);
  if (stream === null) return null;
  if (stream.endsWith(CAMERA_STREAM)) return 'camera';
  if (stream.endsWith(SCREEN_STREAM)) return 'screen';
  return null;
}

/** An SDP's session lines and its sections. */
function split(sdp: string): [string[], string[][]] {
  const head: string[] = [];
  const sections: string[][] = [];
  for (const line of sdpLines(sdp)) {
    if (line.startsWith('m=')) sections.push([line]);
    else if (sections.length > 0) sections.at(-1)!.push(line);
    else head.push(line);
  }
  return [head, sections];
}

/** Whether the side that wrote this direction sends. */
const sends = (direction: string) => direction === 'sendrecv' || direction === 'sendonly';

/** The same direction without this side sending. */
const quiet = (direction: string) => (direction === 'sendrecv' || direction === 'recvonly' ? 'recvonly' : 'inactive');

/** The stream id a video section the other side sends gets on this side. */
const remoteStream = (mid: string) => `teams-${mid}`;

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

/** The port and address a local SDP's media lines name: its first UDP IPv4 candidate's. */
function mediaAddress(sdp: string): [string, string] {
  const found = sdpLines(sdp).map(candidateFields).find((c) => c !== null && c.udp && !c.address.includes(':'));
  return found ? [found.port, found.address] : ['9', '0.0.0.0'];
}

/** An `a=extmap` line's id and URI. */
function extension(line: string): { id: string; uri: string } | null {
  if (!line.startsWith('a=extmap:')) return null;
  const [first, uri] = line.slice('a=extmap:'.length).split(' ');
  if (first === undefined || uri === undefined) return null;
  return { id: first.split('/')[0] ?? first, uri };
}

const extensions = (sdp: string) => sdpLines(sdp).map(extension).filter((e) => e !== null);

const text = (lines: readonly string[]) => lines.map((l) => `${l}\r\n`).join('');

/** A section of a local SDP in Teams' dialect, as `label` under Teams' `mid`; `direction` replaces its own. */
function teamsSection(section: Section, label: Label, mid: string, [port, address]: [string, string], direction: string | null): string {
  const out = [`m=${kindOf(section)} ${port} RTP/SAVP ${formats(section)}`];
  for (const line of section.slice(1)) {
    if (line.startsWith('c=')) out.push(`c=IN IP4 ${address}`);
    else if (line.startsWith('a=rtcp:')) out.push(`a=rtcp:${port}`);
    else if (line.startsWith('a=extmap:')) out.push(line.replaceAll('/', '\\'));
    else if (line === 'a=rtcp-rsize' && label === 'audio') continue;
    else if (line.startsWith('a=candidate:')) {
      const candidate = candidateFields(line);
      if (candidate?.udp) out.push(`a=candidate:${candidate.plain}`);
    } else if (line.startsWith('a=mid:')) {
      out.push(`a=mid:${mid}`, `a=label:${LABELS[label]}`);
      out.push(label === 'audio' ? 'a=x-signaling-fb:* x-message app recv:dsh' : 'a=x-signaling-fb:* x-message app send:src recv:src,vc');
      const ssrc = ssrcOf(section);
      if (ssrc !== null) out.push(`a=x-ssrc-range:${ssrc}-${ssrc}`);
      else if (directionOf(section) === 'recvonly') out.push('a=x-ssrc-range:1-1');
    } else if (direction !== null && isDirection(line)) out.push(`a=${direction}`);
    else out.push(line);
  }
  return text(out);
}

/** What this side tells Teams of a section of its own camera or screen that sends nothing now. */
function toldDirection(section: Section, sending: Sending): string | null {
  const label = own(section);
  if (label === null) return null;
  const quietNow = label === 'camera' ? !sending.camera : label === 'screen' ? !sending.screen : false;
  return quietNow ? quiet(directionOf(section)) : null;
}

/**
 * A section of Teams' in plain WebRTC under `mid`: the profile and extension URIs
 * libwebrtc reads (an extension `keep` refuses left out), UDP candidates without
 * their extras, nothing of Teams' own; a video section the other side sends carries `stream`.
 */
function plainSection(section: Section, mid: string, stream: string | null, keep: (line: string) => boolean): string {
  const video = kindOf(section) === 'video';
  const out = [`m=${kindOf(section)} ${field(section, 1)} UDP/TLS/RTP/SAVPF ${formats(section)}`];
  for (const line of section.slice(1)) {
    if (line.startsWith('a=x-') || line.startsWith('a=label:') || line.startsWith('a=ptime:')) continue;
    if (line.startsWith('a=mid:')) {
      out.push(`a=mid:${mid}`);
      if (stream !== null) out.push(`a=msid:${stream} ${stream}`);
      continue;
    }
    if (video) {
      const ssrc = line.startsWith('a=ssrc:') ? line.slice('a=ssrc:'.length) : null;
      const space = ssrc?.indexOf(' ') ?? -1;
      const attribute = ssrc !== null && space >= 0 ? ssrc.slice(space + 1) : null;
      if (line.startsWith('a=msid:') || (attribute !== null && ['msid:', 'mslabel:', 'label:'].some((a) => attribute.startsWith(a)))) continue;
    }
    if (line.startsWith('a=candidate:')) {
      const fields = line.slice('a=candidate:'.length).split(' ');
      if (fields[2]?.toLowerCase() !== 'udp') continue;
      const end = fields.indexOf('MTURNID');
      out.push(`a=candidate:${(end < 0 ? fields : fields.slice(0, end)).join(' ')}`);
      continue;
    }
    if (line.startsWith('a=extmap:')) {
      const plain = line.replaceAll('\\', '/');
      if (keep(plain)) out.push(plain);
      continue;
    }
    out.push(line);
  }
  return text(out);
}

/** A plain section refused: port 0, nothing sent or received. */
function closed(section: Section): string {
  const out = [`m=${kindOf(section)} 0 ${field(section, 2)} ${formats(section)}`];
  for (const line of section.slice(1)) {
    if (line.startsWith('a=msid:') || line.startsWith('a=candidate:') || line.startsWith('a=ssrc')) continue;
    out.push(isDirection(line) ? 'a=inactive' : line);
  }
  return text(out);
}

/**
 * A section of Teams' that libwebrtc takes no media on (its data channel, a
 * refused one), refused under its mid: libwebrtc then never gives a section of
 * this side's a mid Teams already uses.
 */
function reserved(section: Section, mid: string): string {
  const kind = kindOf(section);
  if (kind !== 'audio' && kind !== 'video') return text(['m=application 0 UDP/DTLS/SCTP webrtc-datachannel', 'c=IN IP4 0.0.0.0', `a=mid:${mid}`]);
  const codecs = section.slice(1).filter((l) => l.startsWith('a=rtpmap:') || l.startsWith('a=fmtp:'));
  return text([`m=${kind} 0 UDP/TLS/RTP/SAVPF ${formats(section)}`, 'c=IN IP4 0.0.0.0', ...codecs, `a=mid:${mid}`, 'a=inactive']);
}

const TRANSPORT = ['a=setup:', 'a=ice-ufrag:', 'a=ice-pwd:', 'a=fingerprint:', 'a=ice-options:'];

/** libwebrtc's data channel section in Teams' dialect, under `mid`. The source range it names is made up: no RTP goes on it. */
function teamsData(section: Section, mid: string, [port, address]: [string, string]): string {
  const out = [`m=x-data ${port} RTP/SAVP 127`, `c=IN IP4 ${address}`, 'a=x-data-protocol:sctp'];
  let hash = 2_166_136_261;
  for (const byte of new TextEncoder().encode(after(section, 'a=ice-ufrag:') ?? mid)) hash = Math.imul(hash ^ byte, 16_777_619) >>> 0;
  const source = (hash | 1) >>> 0;
  out.push(`a=x-ssrc-range:${source}-${source}`, 'a=rtpmap:127 x-data/90000');
  for (const line of section.slice(1)) {
    if (TRANSPORT.some((p) => line.startsWith(p))) out.push(line);
    else {
      const candidate = candidateFields(line);
      if (candidate?.udp) out.push(`a=candidate:${candidate.plain}`);
    }
  }
  out.push(`a=mid:${mid}`, 'a=sendrecv', 'a=rtcp-mux', 'a=label:data');
  return text(out);
}

/** Teams' data section as libwebrtc's SCTP data channel, under `mid`. */
function plainData(section: Section, mid: string): string {
  const out = [`m=application ${field(section, 1)} UDP/DTLS/SCTP webrtc-datachannel`];
  for (const line of section.slice(1)) {
    if (line.startsWith('c=') || TRANSPORT.some((p) => line.startsWith(p))) out.push(line);
    else if (line.startsWith('a=candidate:')) {
      const fields = line.slice('a=candidate:'.length).split(' ');
      if (fields[2]?.toLowerCase() !== 'udp') continue;
      const end = fields.indexOf('MTURNID');
      out.push(`a=candidate:${(end < 0 ? fields : fields.slice(0, end)).join(' ')}`);
    }
  }
  out.push(`a=mid:${mid}`, 'a=sctp-port:5000', 'a=max-message-size:262144');
  return text(out);
}

/** Teams' session lines in plain WebRTC, bundling `mids`. */
function plainHead(head: readonly string[], mids: readonly string[]): string {
  const out: string[] = [];
  for (const line of head) {
    if (line.startsWith('a=group:') || line.startsWith('a=x-')) continue;
    out.push(line);
    if (line.startsWith('t=')) out.push(`a=group:BUNDLE ${mids.join(' ')}`);
  }
  return text(out);
}

/** A local SDP's session lines for Teams, bundling `mids`. */
function teamsHead(head: readonly string[], mids: readonly string[]): string {
  return text(head.map((line) => (line.startsWith('a=group:BUNDLE') ? `a=group:BUNDLE ${mids.join(' ')}` : line)));
}

/**
 * This side's plain offer in Teams' dialect: its audio, the camera section and the
 * screen section, a section of this side's own taking the label over from one
 * that only received; a refused section is left out. The offer waits in `layout`
 * for Teams' answer (`plainAnswer`).
 */
export function teamsOffer(sdp: string, sending: Sending, layout: Layout): string {
  const [head, sections] = split(sdp);
  const address = mediaAddress(sdp);
  const labelled = (s: Section): Label | null => {
    const mid = midOf(s);
    return own(s) ?? (mid === null ? undefined : layout.labels.get(mid)) ?? null;
  };
  let spareScreen = -1;
  if (layout.group && !sections.some((s) => isOpen(s) && labelled(s) === 'screen')) {
    spareScreen = sections.findLastIndex((s) => isOpen(s) && kindOf(s) === 'video' && labelled(s) === null);
  }
  const kept: [number, Label | null][] = [];
  let audio = false;
  sections.forEach((section, at) => {
    if (!isOpen(section)) return;
    let label: Label;
    if (kindOf(section) === 'audio' && !audio) {
      audio = true;
      label = 'audio';
    } else if (kindOf(section) === 'video') label = at === spareScreen ? 'screen' : labelled(section) ?? 'camera';
    else if (kindOf(section) === 'application' && !kept.some(([, l]) => l === null)) {
      kept.push([at, null]);
      return;
    } else return;
    const i = kept.findIndex(([, l]) => l === label);
    if (i >= 0 && layout.group && label === 'camera') kept.push([at, label]);
    else if (i >= 0 && own(section) !== null) kept[i] = [at, label];
    else if (i < 0) kept.push([at, label]);
  });
  const mids: string[] = [];
  let body = '';
  for (const [at, label] of kept) {
    const section = sections[at]!;
    const mid = midOf(section);
    if (mid === null) continue;
    mids.push(mid);
    if (label === null) {
      body += teamsData(section, mid, address);
      continue;
    }
    if (label !== 'audio') layout.labels.set(mid, label);
    body += teamsSection(section, label, mid, address, toldDirection(section, sending));
  }
  layout.offered = sdp;
  return teamsHead(head, mids) + body;
}

/**
 * Teams' answer to this side's offer waiting in `layout`, in plain WebRTC: a
 * section for each of the offer's, refused where Teams' answer has none. Null
 * with no offer waiting, or no audio answered.
 */
export function plainAnswer(sdp: string, layout: Layout): string | null {
  const offer = layout.offered;
  if (offer === null) return null;
  layout.offered = null;
  const [head, answered] = split(sdp);
  const [, offered] = split(offer);
  for (const section of answered) {
    const mid = midOf(section);
    const id = streamIdOf(section);
    if (mid !== null && id !== null) layout.streams.set(mid, id);
  }
  let body = '';
  const bundle: string[] = [];
  let audio = false;
  for (const section of offered) {
    const mid = midOf(section) ?? '';
    const kind = kindOf(section);
    const usable = (a: Section) => isOpen(a) && isOpen(section);
    if (kind === 'application') {
      const data = answered.find((a) => usable(a) && kindOf(a) === 'x-data' && midOf(a) === mid);
      if (data === undefined) body += closed(section);
      else {
        body += plainData(data, mid);
        bundle.push(mid);
      }
      continue;
    }
    let answer = answered.filter((a) => usable(a) && kindOf(a) === kind).find((a) => midOf(a) === mid)
      ?? answered.find((a) => usable(a) && kindOf(a) === 'audio' && kind === 'audio') ?? null;
    if (kind === 'audio' && audio) answer = null;
    if (answer !== null && (kindOf(answer) === 'audio' || kindOf(answer) === 'video')) {
      audio ||= kindOf(answer) === 'audio';
      const stream = kindOf(answer) === 'video' && sends(directionOf(answer)) ? remoteStream(mid) : null;
      body += plainSection(answer, mid, stream, () => true);
      bundle.push(mid);
    } else body += closed(section);
  }
  if (!audio) return null;
  const plain = plainHead(head, bundle) + body;
  layout.remote = plain;
  return plain;
}

/**
 * An offer of Teams' in plain WebRTC, made to fit the call's sections: each the
 * call has keeps its place, the audio its mid whatever Teams numbers it, a video
 * section under a new mid comes after them, and one Teams no longer lists is
 * refused. libwebrtc also refuses an extension id given another URI (Teams' hold
 * offer moves id 3 from transport-wide-cc to its own feedback extension): such an
 * extension is left out, so the answer declines it. Null without audio.
 */
export function plainOffer(sdp: string, layout: Layout): string | null {
  const previous = layout.remote;
  layout.remote = '';
  const [, slots] = split(previous);
  const active = extensions(previous);
  const keep = (line: string) => {
    const own = extension(line);
    return own === null || !active.some((other) => (other.id === own.id) !== (other.uri === own.uri));
  };
  const [head, sections] = split(sdp);
  const out: [string, string, boolean][] = slots.map((s) => [midOf(s) ?? '', closed(s), false]);
  const audioSlot = slots.findIndex((s) => kindOf(s) === 'audio');
  let audio = false;
  const dataSlot = slots.findIndex((s) => kindOf(s) === 'application' && isOpen(s));
  layout.mids.clear();
  for (const section of sections) {
    const theirs = midOf(section);
    if (theirs === null) continue;
    if (isOpen(section) && kindOf(section) === 'x-data' && dataSlot >= 0) {
      const mid = out[dataSlot]![0];
      layout.mids.set(theirs, mid);
      const id = streamIdOf(section);
      if (id !== null) layout.streams.set(mid, id);
      out[dataSlot] = [mid, plainData(section, mid), true];
      continue;
    }
    if (!isOpen(section) || (kindOf(section) !== 'audio' && kindOf(section) !== 'video')) {
      if (!out.some(([mid]) => mid === theirs)) out.push([theirs, reserved(section, theirs), false]);
      continue;
    }
    let slot: number | null;
    let mid: string;
    let label: Label;
    if (kindOf(section) === 'audio' && !audio) {
      audio = true;
      label = 'audio';
      [slot, mid] = audioSlot >= 0 ? [audioSlot, out[audioSlot]![0]] : [null, theirs];
    } else if (kindOf(section) === 'video') {
      label = labelIn(section) ?? 'camera';
      const i = slots.findIndex((s) => midOf(s) === theirs);
      if (i >= 0 && kindOf(slots[i]!) === 'video') [slot, mid] = [i, theirs];
      else if (i >= 0) [slot, mid] = [null, `${theirs}v`];
      else [slot, mid] = [null, theirs];
    } else continue;
    layout.mids.set(theirs, mid);
    if (label !== 'audio') layout.labels.set(mid, label);
    const id = streamIdOf(section);
    if (id !== null) layout.streams.set(mid, id);
    const stream = kindOf(section) === 'video' && sends(directionOf(section)) ? remoteStream(mid) : null;
    const plain = plainSection(section, mid, stream, keep);
    if (slot !== null) out[slot] = [mid, plain, true];
    else out.push([mid, plain, true]);
  }
  if (!audio) return null;
  const bundle = out.filter(([, , open]) => open).map(([mid]) => mid);
  const plain = plainHead(head, bundle) + out.map(([, section]) => section).join('');
  layout.remote = plain;
  layout.offered = null;
  return plain;
}

/**
 * A local answer to Teams' `offer` (made plain by `plainOffer`) in Teams' dialect:
 * the offer's sections in its order under its mids, each with no plain answer
 * refused with port 0, as its own client does.
 */
export function teamsAnswer(answer: string, offer: string, sending: Sending, layout: Layout): string {
  const [head, answered] = split(answer);
  const address = mediaAddress(answer);
  const [, offered] = split(offer);
  let body = '';
  const bundle: string[] = [];
  for (const section of offered) {
    const theirs = midOf(section) ?? '';
    const plain = layout.mids.get(theirs);
    const found = plain === undefined ? undefined : answered.find((a) => isOpen(a) && midOf(a) === plain);
    if (found !== undefined && kindOf(found) === 'application') {
      bundle.push(theirs);
      body += teamsData(found, theirs, address);
    } else if (found !== undefined) {
      const label: Label = kindOf(found) === 'audio' ? 'audio' : (plain === undefined ? undefined : layout.labels.get(plain)) ?? 'camera';
      bundle.push(theirs);
      body += teamsSection(found, label, theirs, address, toldDirection(found, sending));
    } else {
      const [kind = '', , ...rest] = section[0]!.split(' ');
      body += `${kind} 0 ${rest.join(' ')}\r\n`;
      if (midOf(section) !== null) body += `a=mid:${theirs}\r\n`;
    }
  }
  return teamsHead(head, bundle) + body;
}

/** The videos the other side sends now: each section's stream id on this side and what it carries. */
export function remoteVideos(layout: Layout): [string, Label][] {
  const out: [string, Label][] = [];
  for (const section of split(layout.remote)[1]) {
    if (kindOf(section) !== 'video' || !isOpen(section)) continue;
    const stream = streamOf(section);
    const mid = midOf(section);
    if (stream !== null && mid !== null) out.push([stream, layout.labels.get(mid) ?? 'camera']);
  }
  return out;
}

/** The video sections Teams sends on, with the stream id it gave each. */
export function videoSlots(layout: Layout): VideoSlot[] {
  return split(layout.remote)[1].flatMap((s) => {
    if (kindOf(s) !== 'video' || !isOpen(s)) return [];
    const [stream, mid] = [streamOf(s), midOf(s)];
    const streamId = mid === null ? undefined : layout.streams.get(mid);
    if (stream === null || mid === null || streamId === undefined) return [];
    return [{ streamId, stream, label: layout.labels.get(mid) ?? 'camera' }];
  });
}

/** The stream id Teams gave the data channel's section: this side's address on it. */
export function dataStream(layout: Layout): number | null {
  const section = split(layout.remote)[1].find((s) => kindOf(s) === 'application' && isOpen(s));
  const mid = section === undefined ? null : midOf(section);
  return (mid === null ? undefined : layout.streams.get(mid)) ?? null;
}

/** The call's modalities as Teams' requests name them. */
export function modalities(layout: Layout, sending: Sending): string[] {
  const remote = remoteVideos(layout);
  const out = ['Audio'];
  if (sending.camera || remote.some(([, l]) => l === 'camera')) out.push('Video');
  if (sending.screen) out.push('ScreenSharer');
  else if (remote.some(([, l]) => l === 'screen')) out.push('ScreenViewer');
  return out;
}

/** `sources`: the cameras and screens its endpoints have, sending or not (a group call's). */
export type RosterChange = { mri: string; name: string; present: boolean; sources: Source[] };
export type CallPushEvent =
  | { kind: 'accepted'; sdp: string; acknowledge: string | null; callLeg: string | null; renegotiation: string | null }
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

/** Where this side's own new offers go: the link a pushed acceptance names, or the one the acceptance of a call answered here comes back with. */
export function renegotiationLink(answer: Doc): string | null {
  const links = (doc: unknown) => ((doc as Doc | undefined)?.links as Doc | undefined)?.mediaRenegotiation;
  const found = [links(answer), links(answer.callAcceptanceAcknowledgement)].find((v) => typeof v === 'string');
  return typeof found === 'string' && found.startsWith('https://') ? found : null;
}

export function isCallCallback(envelope: Doc, endpoint: string): boolean {
  return typeof envelope.url === 'string' && envelope.url.includes(`callAgent/${endpoint}/`);
}

/** An endpoint's videos, from its `call.mediaStreams`. */
function sourcesOf(endpoint: unknown): Source[] {
  const streams = ((endpoint as Doc | null)?.call as Doc | undefined)?.mediaStreams;
  if (!Array.isArray(streams)) return [];
  return streams.flatMap((stream: Doc) => {
    const label = stream?.label;
    if (label !== 'main-video' && label !== 'applicationsharing-video') return [];
    const id = stream.sourceId;
    if (typeof id !== 'number' || !Number.isSafeInteger(id)) return [];
    return [{ id, screen: label === 'applicationsharing-video', live: stream.direction === 'sendrecv' || stream.direction === 'sendonly' }];
  });
}

export function callPushEvent(envelope: Doc): CallPushEvent {
  const body = bodyOf(envelope);
  if (body === null) return { kind: 'other' };
  const accepted = (body.callAcceptance ?? body.mediaAnswer) as Doc | undefined;
  const blob = (accepted?.mediaContent as Doc | undefined)?.blob;
  if (typeof blob === 'string') {
    const links = accepted?.links as Doc | undefined;
    const text = (v: unknown) => (typeof v === 'string' ? v : null);
    return { kind: 'accepted', sdp: blob, acknowledge: text(links?.acknowledgement) ?? text(links?.mediaAcknowledgement), callLeg: text(links?.callLeg), renegotiation: renegotiationLink(accepted!) };
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
      const object = typeof endpoints === 'object' && endpoints !== null && !Array.isArray(endpoints) ? (endpoints as Record<string, unknown>) : null;
      return {
        mri,
        name: typeof name === 'string' ? name : '',
        present: object !== null && Object.keys(object).length > 0,
        sources: object === null ? [] : Object.values(object).flatMap(sourcesOf),
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

const conversationRequest = (c: Caller, conversationType: string | null, suppressDialout: boolean): Doc => {
  const cb = (path: string) => callback(c, path);
  return {
    conversationType, subject: null, suppressDialout,
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
  };
};

const callInvitation = (c: Caller, offer: string): Doc => {
  const cb = (path: string) => callback(c, path);
  return {
    callModalities: ['Audio'], replaces: null, transferor: null,
    links: { progress: cb('call/progress/'), mediaAnswer: cb('call/mediaAnswer/'), acceptance: cb('call/acceptance/'), redirection: cb('call/redirection/'), end: cb('call/end/') },
    clientContentForMediaController: { controlVideoStreaming: cb('call/controlVideoStreaming/'), csrcInfo: cb('call/csrcInfo/') },
    pstnContent: { emergencyCallCountry: '', platformName: CALL_CLIENT, publicApiCall: false },
    mediaContent: { blob: offer, contentType: 'application/sdp-ngc-1.0', requiredFeatures: 'nonByPass' },
  };
};

const endpointState = (sequence: number): Doc => ({
  endpointStateSequenceNumber: sequence,
  endpointProperties: { additionalEndpointProperties: { infoShownInReportMode: 'FullInformation' } },
});

const ENDPOINT = { capabilities: null, endpointCapabilities: 73463, clientEndpointCapabilities: 114259690, endpointMetadata: { holographicCapabilities: 3 } };

/** `epconv`'s body placing a call to `to`, a group's with its `thread`. */
export function placeCallBody(c: Caller, to: readonly string[], thread: string | null, offer: string, target: string): Doc {
  return {
    conversationRequest: conversationRequest(c, null, false),
    groupContext: null,
    groupChat: thread === null ? null : { threadId: thread, messageId: null },
    participants: { from: fromOf(c), to: to.map((id) => ({ id, participantId: target })) },
    ...ENDPOINT,
    meetingInfo: null,
    endpointState: endpointState(1),
    callInvitation: callInvitation(c, offer),
  };
}

/** `epconv`'s body starting a channel's call, as Teams' Meet in a channel does: it rings no one. */
export function startChannelCallBody(c: Caller, thread: string, offer: string): Doc {
  return {
    conversationRequest: conversationRequest(c, 'huddleGroupCall', true),
    groupContext: null,
    groupChat: { threadId: thread, messageId: null },
    participants: { from: fromOf(c), to: [] },
    ...ENDPOINT,
    meetingInfo: null,
    endpointState: endpointState(1),
    callInvitation: callInvitation(c, offer),
  };
}

/** A meeting's call, or a channel's: its thread, the message it hangs off (`0` for the thread's own) and its organizer. */
export type MeetingJoin = { thread: string; message: string; organizer: string; tenant: string };

/**
 * The call to join in a thread, from its threads document (`threadProperties=1`):
 * the one running (`awareness_conversationLiveState:*`, `Active`), else the
 * thread's meeting (`meeting`); null when it has neither. Mirrors `meeting_join`
 * in `crates/rv-teams/src/calling.rs`.
 */
export function meetingJoin(doc: Doc, thread: string, tenant: string): MeetingJoin | null {
  const properties = (doc.properties ?? doc.threadProperties) as Doc | undefined;
  if (typeof properties !== 'object' || properties === null) return null;
  const parsed = (v: unknown): Doc | null => {
    if (typeof v === 'object' && v !== null && !Array.isArray(v)) return v as Doc;
    if (typeof v !== 'string') return null;
    try {
      const value: unknown = JSON.parse(v);
      return typeof value === 'object' && value !== null && !Array.isArray(value) ? value as Doc : null;
    } catch {
      return null;
    }
  };
  const text = (d: Doc | null | undefined, key: string) => {
    const v = d?.[key];
    return typeof v === 'string' && v !== '' ? v : null;
  };
  const live = Object.entries(properties)
    .filter(([key]) => key.startsWith('awareness_conversationLiveState'))
    .map(([, v]) => parsed(v))
    .find((state) => state?.status === 'Active') ?? null;
  const meeting = parsed(properties.meeting);
  let organizer: string | null;
  let tenantId: string | null;
  let message: string | null = null;
  if (live !== null) {
    const info = parsed(live.meetingInfo);
    const initiator = text(live, 'groupCallInitiator');
    organizer = text(info, 'organizerId') ?? (initiator?.startsWith('8:orgid:') ? initiator.slice(8) : null);
    tenantId = text(info, 'tenantId');
  } else if (meeting !== null) {
    organizer = text(meeting, 'organizerId');
    tenantId = text(meeting, 'tenantId');
    const link = text(meeting, 'meetingJoinUrl');
    message = link === null ? null : joinMessage(link);
  } else return null;
  if (organizer === null || organizer === '') return null;
  return { thread, message: message ?? '0', organizer, tenant: tenantId ?? tenant };
}

/** The message a meeting link joins (`/l/meetup-join/<thread>/<message>`). */
export function joinMessage(link: string): string | null {
  let path: string[];
  try {
    path = new URL(link).pathname.split('/').filter((p) => p !== '');
  } catch {
    return null;
  }
  return path[0] === 'l' && path[1] === 'meetup-join' && (path[3] ?? '') !== '' ? path[3]! : null;
}

/** `epconv`'s body resolving a meeting's conversation, before its call is joined. */
export function joinMeetingBody(c: Caller, join: MeetingJoin): Doc {
  const { conversationType: _type, suppressDialout: _dialout, ...request } = conversationRequest(c, null, false);
  return {
    conversationRequest: request,
    groupContext: null,
    groupChat: { threadId: join.thread, messageId: join.message },
    participants: { from: fromOf(c) },
    ...ENDPOINT,
    meetingInfo: { tenantId: join.tenant, organizerId: join.organizer },
    endpointState: endpointState(0),
  };
}

/** The conversation controller's body joining the meeting's call with this offer. */
export function joinMeetingCallBody(c: Caller, join: MeetingJoin, offer: string): Doc {
  return {
    conversationRequest: conversationRequest(c, null, true),
    contentSharing: null,
    participants: { from: fromOf(c), to: [] },
    ...ENDPOINT,
    groupContext: null,
    groupChat: { threadId: join.thread, messageId: join.message },
    meetingInfo: { organizerId: join.organizer, tenantId: join.tenant },
    endpointState: endpointState(1),
    callInvitation: callInvitation(c, offer),
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
export function answerNegotiationBody(answer: string, mediaLeg: string | null, callModalities: readonly string[]): Doc {
  return { mediaAnswer: { callModalities, mediaContent: { blob: answer, contentType: 'application/sdp-ngc-1.0', mediaLegId: mediaLeg, escalationOccurring: false, newOffer: false } } };
}

/**
 * `renegotiate`'s body: this side's new offer, as Teams' web client sends one when
 * its camera or its shared screen starts or stops (`screen`). Teams' answer comes
 * to the `mediaAnswer` callback.
 */
export function renegotiateBody(c: Caller, offer: string, mediaLeg: string | null, callModalities: readonly string[], screen: boolean): Doc {
  const cb = (path: string) => callback(c, path);
  return {
    mediaNegotiation: {
      callModalities,
      sender: fromOf(c),
      links: { mediaAnswer: cb('call/mediaAnswer/'), rejection: cb('call/rejection/') },
      mediaContent: { blob: offer, contentType: 'application/sdp-ngc-1.0', requiredFeatures: 'nonByPass', negotiationTag: `${c.participant};${screen ? 'ss' : 'v'}_1`, mediaLegId: mediaLeg },
    },
  };
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
  const layout = new Layout();
  const offer = teamsOffer(await engine.offer(), sendingOf(engine), layout);
  const pushes = listen(deps);
  let created: Doc;
  try {
    created = await deps.post(EPCONV, chain, placeCallBody(caller, to, thread, offer, deps.id()));
  } catch (error) {
    pushes.stop();
    throw error;
  }
  runCall(deps, engine, caller, chain, (created.links ?? {}) as Doc, pushes, newMedia(layout));
}

/**
 * Joins a channel's or a meeting chat's call on `engine`, as Teams' web client
 * does: `join` (the running call, else the meeting), or with none starts the
 * channel's call. Resolves once Teams took it.
 */
export async function joinThreadCall(deps: CallDeps, engine: ExternalCallEngine, thread: string, join: MeetingJoin | null): Promise<void> {
  const caller: Caller = { ...deps.me, endpoint: deps.id(), participant: deps.id(), surl: deps.surl, random: deps.id() };
  const chain = deps.id();
  await engine.start(caller.endpoint, { dataChannel: DATA_LABEL, receiveVideos: GROUP_VIDEOS });
  const layout = Layout.group();
  const offer = teamsOffer(await engine.offer(), sendingOf(engine), layout);
  const pushes = listen(deps);
  let created: Doc;
  try {
    if (join === null) created = await deps.post(EPCONV, chain, startChannelCallBody(caller, thread, offer));
    else {
      const controller = (await deps.post(EPCONV, chain, joinMeetingBody(caller, join))).conversationController;
      if (typeof controller !== 'string') throw new Error('call_join_failed');
      created = await deps.post(controller, chain, joinMeetingCallBody(caller, join, offer));
    }
  } catch (error) {
    pushes.stop();
    throw error;
  }
  runCall(deps, engine, caller, chain, (created.links ?? {}) as Doc, pushes, newMedia(layout));
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
export function acceptBody(c: Caller, answer: string, mediaLeg: string | null, callModalities: readonly string[]): Doc {
  const cb = (path: string) => callback(c, path);
  return {
    callAcceptance: {
      acceptedBy: fromOf(c),
      acceptedCallModalities: callModalities,
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
  const layout = new Layout();
  const plain = plainOffer(ring.offer, layout);
  if (plain === null) throw new Error('call_offer_invalid');
  const pushes = listen(deps);
  try {
    const links = attachedLinks(await deps.post(ring.attach, chain, attachBody(caller, ring.conversation)));
    if (links === null || typeof links.acceptance !== 'string') throw new Error('call_attach_failed');
    await engine.start(caller.endpoint);
    const local = await engine.answerOffer(plain);
    if (local === null) throw new Error('call_answer_failed');
    const sending = sendingOf(engine);
    const accepted = await deps.post(links.acceptance, chain, acceptBody(caller, teamsAnswer(local, ring.offer, sending, layout), ring.mediaLeg, modalities(layout, sending)));
    runCall(deps, engine, caller, chain, links, pushes, { layout, answered: true, renegotiation: renegotiationLink(accepted), mediaLeg: ring.mediaLeg });
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

/** What a call's media negotiated so far, as the run takes it over. */
type Media = {
  layout: Layout;
  /** Whether the other side took the call already: one answered here. */
  answered: boolean;
  /** Where this side's own new offers go, once Teams said. */
  renegotiation: string | null;
  mediaLeg: string | null;
};

const newMedia = (layout: Layout): Media => ({ layout, answered: false, renegotiation: null, mediaLeg: null });

const sendingOf = (engine: ExternalCallEngine): Sending => engine.sending?.() ?? NOT_SENDING;

type Video = { stream: string; member: string; screen: boolean };

/** A group call's media server, spoken to on the data channel: which video it sends on which section. */
type Group = { channel: Channel; routing: Routing };

/**
 * The call's run until it ends: the broker lease, the endpoint metadata, the
 * answer (when placed here), this side's new offers and Teams' answers to them,
 * Teams' own offers, the roster, the videos and the end. One step at a time, in
 * the order they came.
 */
function runCall(deps: CallDeps, engine: ExternalCallEngine, caller: Caller, chain: string, links: Doc, pushes: Pushes, media: Media): void {
  const leaveLink = links.leave;
  const leave = () => { if (typeof leaveLink === 'string') void deps.post(leaveLink, chain, leaveBody(caller)).catch(() => {}); };
  let done = false;
  const { layout } = media;
  let { answered, renegotiation, mediaLeg } = media;
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
  const steps: (() => Promise<void>)[] = [];
  let running = false;
  const step = (task: () => Promise<void>) => {
    steps.push(task);
    if (running) return;
    running = true;
    void (async () => {
      while (steps.length > 0 && !done) await steps.shift()!().catch(() => {});
      steps.length = 0;
      running = false;
    })();
  };
  let unsubscribe = () => {};
  const finish = (reason: string | null) => {
    if (done) return;
    done = true;
    clearTimeout(ringing);
    if (heartbeat !== null) clearInterval(heartbeat);
    pushes.stop();
    unsubscribe();
    if (reason !== null) void engine.ended(reason);
  };
  const ringing = setTimeout(() => { if (!answered) { finish('no_answer'); leave(); } }, RINGING_MS);
  const roster = new Map<string, string>();
  const sources = new Map<string, Source[]>();
  let group: Group | null = null;
  let heartbeat: ReturnType<typeof setInterval> | null = null;
  let shown: Video[] = [];
  const showVideos = async () => {
    const member = roster.size === 1 ? [...roster.keys()][0]! : null;
    const videos = group !== null ? group.routing.shown(videoSlots(layout))
      : member === null ? [] : remoteVideos(layout).map(([stream, label]) => ({ stream, member, screen: label === 'screen' }));
    if (JSON.stringify(videos) === JSON.stringify(shown)) return;
    shown = videos;
    await engine.videos?.(videos);
  };
  const route = async () => {
    if (group === null) return;
    for (const [stream, source, screen] of group.routing.plan(videoSlots(layout), sources)) await engine.sendData?.(group.channel.subscribe(stream, source, screen));
  };
  const greet = async () => {
    const address = dataStream(layout);
    if (address === null || engine.sendData === undefined) return;
    const channel = new Channel(address);
    await engine.sendData(channel.syn());
    group = { channel, routing: new Routing() };
    await route();
    if (done) return;
    const started = group;
    heartbeat = setInterval(() => { void engine.sendData?.(started.channel.heartbeat()).catch(() => {}); }, 1000);
  };
  let waiting: string | null = null;
  let sharing = false;
  const offerAnew = async (sdp: string) => {
    if (renegotiation === null) return;
    const sending = sendingOf(engine);
    const offer = teamsOffer(sdp, sending, layout);
    const screen = sharing !== sending.screen;
    sharing = sending.screen;
    try {
      await deps.post(renegotiation, chain, renegotiateBody(caller, offer, mediaLeg, modalities(layout, sending), screen));
    } catch {
      layout.withdraw();
    }
  };
  unsubscribe = engine.offers?.((sdp) => {
    if (done) return;
    if (!answered) { waiting = sdp; return; }
    step(() => offerAnew(sdp));
  }) ?? (() => {});
  const handle = async (event: CallPushEvent) => {
    if (event.kind === 'negotiation' && engine.answerOffer !== undefined) {
      layout.withdraw();
      if (event.mediaLeg !== null) mediaLeg = event.mediaLeg;
      const plain = plainOffer(event.sdp, layout);
      if (plain === null) return;
      const local = await engine.answerOffer(plain);
      if (local === null || done) return;
      const sending = sendingOf(engine);
      const answer = teamsAnswer(local, event.sdp, sending, layout);
      await deps.post(event.answerLink, chain, answerNegotiationBody(answer, mediaLeg, modalities(layout, sending))).catch(() => {});
      await route();
      await showVideos();
    } else if (event.kind === 'accepted' && answered && layout.offering()) {
      const answer = plainAnswer(event.sdp, layout);
      if (answer === null) return;
      await engine.answer(answer);
      if (event.acknowledge !== null) await deps.post(event.acknowledge, chain, {}).catch(() => {});
      await route();
      await showVideos();
    } else if (event.kind === 'accepted' && !answered) {
      const answer = plainAnswer(event.sdp, layout);
      if (answer === null) { finish('connect_failed'); leave(); return; }
      answered = true;
      renegotiation = event.renegotiation;
      await engine.answer(answer);
      await greet();
      if (event.acknowledge !== null) {
        const acknowledged = await deps.post(event.acknowledge, chain, acknowledgeBody(caller)).catch(() => null);
        if (renegotiation === null && acknowledged !== null) renegotiation = renegotiationLink(acknowledged);
      }
      await showVideos();
      if (event.callLeg !== null) await deps.post(event.callLeg, chain, registerOnLegBody(caller)).catch(() => {});
      const offer = waiting;
      waiting = null;
      if (offer !== null) await offerAnew(offer);
    } else if (event.kind === 'roster') {
      for (const change of event.changes) {
        if (change.mri.toLowerCase() === caller.mri.toLowerCase()) continue;
        if (change.present) {
          sources.set(change.mri, change.sources);
          roster.set(change.mri, change.name);
        } else {
          sources.delete(change.mri);
          roster.delete(change.mri);
        }
      }
      await route();
      await engine.roster([...roster].map(([id, name]) => ({ id, name, muted: false })));
      await showVideos();
    } else if (event.kind === 'ended') finish('remote_ended');
  };
  pushes.attach((envelope) => {
    if (done || !isCallCallback(envelope, caller.endpoint)) return;
    const event = callPushEvent(envelope);
    step(() => handle(event));
  });
  void engine.left().then(() => { if (!done) { finish(null); leave(); } });
}
