/**
 * A Teams group call's data channel (`datachannel.rs` in crates/rv-teams;
 * brain/architecture/native-calls.md): its media server sends a video only on a
 * section this side asked for it by name. Each message is a frame, `0x10`, the
 * header's remaining length, a kind (`0x92` for the JSON this side speaks),
 * `0x00`, a little-endian sequence number, the sender's stream id, the
 * receivers' count and stream ids (all big-endian), `0x01`, then a JSON array.
 */

import type { Label } from './calling.ts';

/** The label the web client gives the channel. */
export const LABEL = 'main-channel';

/** The media server's own address on the channel. */
const SERVER = 0xfffffffc;
const JSON_KIND = 0x92;

/** A video section Teams sends on, in a group call: what its media server is asked to send there names it by `streamId`. */
export type VideoSlot = {
  streamId: number;
  /** The stream id its track has on this side. */
  stream: string;
  label: Label;
};

/** A video someone in the call sends, by the roster's stream id. */
export type Source = {
  id: number;
  screen: boolean;
  /** Whether it sends now. */
  live: boolean;
};

const u32be = (n: number) => [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];

/** This side of the channel: its address (the data section's stream id) and the counters its messages carry. */
export class Channel {
  private readonly address: number;
  private sent = 0;
  private requests = 0;

  constructor(address: number) {
    this.address = address;
  }

  private frame(message: unknown): Uint8Array {
    const head = [0x10, 0x0f, JSON_KIND, 0x00, this.sent & 0xff, (this.sent >>> 8) & 0xff, ...u32be(this.address), 1, ...u32be(SERVER), 1];
    const body = new TextEncoder().encode(JSON.stringify([message]));
    this.sent = (this.sent + 1) & 0xffff;
    const out = new Uint8Array(head.length + body.length);
    out.set(head);
    out.set(body, head.length);
    return out;
  }

  /** The first message, with what this side takes and sends. */
  syn(): Uint8Array {
    return this.frame({ client_capabilities: ['dsh', 'bwe', 'sr', 'ssbwe', 'leave', 'vdclc', 'heartbeat'], type: 'syn' });
  }

  /** Sent every second, as the server sends its own. */
  heartbeat(): Uint8Array {
    return this.frame({ state: 0, type: 'heartbeat' });
  }

  /** Asks for `source` (a roster's stream id) on the section of `stream`, or for nothing there with null. */
  subscribe(stream: number, source: number | null, screen: boolean): Uint8Array {
    this.requests += 1;
    const format = screen
      ? { 'max-fps': 1500, 'max-fs': 8160, 'max-mbps': 135000, 'profile-level-id': '64001f' }
      : { 'max-fps': 3000, 'max-fs': 8160, 'max-mbps': 244800, 'profile-level-id': '64001f' };
    return this.frame({
      controlVideoStreaming: {
        controlInfo: { fmtParams: [format], sourceId: source ?? -1, streamMsid: stream },
        sequenceNumber: this.requests,
      },
      type: 'sr',
    });
  }
}

/** The JSON messages of one frame; none for a frame of another kind. */
export function messages(frame: Uint8Array): Record<string, unknown>[] {
  if (frame.length < 4 || frame[0] !== 0x10 || frame[2] !== JSON_KIND) return [];
  const start = frame[1]! + 1;
  if (start > frame.length) return [];
  try {
    const parsed = JSON.parse(new TextDecoder().decode(frame.subarray(start))) as unknown;
    return Array.isArray(parsed) ? (parsed as Record<string, unknown>[]) : [];
  } catch {
    return [];
  }
}

/** A request: the section's stream id, the source asked for there (null for none), and whether a screen's. */
export type Request = [number, number | null, boolean];

/** Which video goes on which of this side's sections. */
export class Routing {
  /** The source on each section, by the section's stream id, and whose it is. */
  private routed = new Map<number, [number, string]>();

  /**
   * The requests that bring `sources` (by whose they are) onto `slots`: a source
   * that stopped leaves its section, a new one takes a free section of its kind,
   * in order. Sources past the free sections are not shown.
   */
  plan(slots: readonly VideoSlot[], sources: ReadonlyMap<string, readonly Source[]>): Request[] {
    const live: [string, Source][] = [...sources.keys()].sort()
      .flatMap((who) => sources.get(who)!.filter((s) => s.live).map((s): [string, Source] => [who, s]));
    const requests: Request[] = [];
    for (const id of [...this.routed.keys()].sort((a, b) => a - b)) {
      const [source] = this.routed.get(id)!;
      const slot = slots.find((s) => s.streamId === id);
      if (slot === undefined || !live.some(([, s]) => s.id === source)) {
        this.routed.delete(id);
        if (slot !== undefined) requests.push([id, null, slot.label === 'screen']);
      }
    }
    for (const [who, source] of live) {
      if ([...this.routed.values()].some(([id]) => id === source.id)) continue;
      const label: Label = source.screen ? 'screen' : 'camera';
      const slot = slots.find((s) => s.label === label && !this.routed.has(s.streamId));
      if (slot === undefined) continue;
      this.routed.set(slot.streamId, [source.id, who]);
      requests.push([slot.streamId, source.id, source.screen]);
    }
    return requests;
  }

  /** Whose video each section shows: its stream on this side, whose, and whether it is a screen. */
  shown(slots: readonly VideoSlot[]): { stream: string; member: string; screen: boolean }[] {
    return slots.flatMap((slot) => {
      const routed = this.routed.get(slot.streamId);
      return routed === undefined ? [] : [{ stream: slot.stream, member: routed[1], screen: slot.label === 'screen' }];
    });
  }
}
