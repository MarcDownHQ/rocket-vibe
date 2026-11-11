/**
 * Mattermost's Calls plugin (`com.mattermost.calls`), carried by the voice
 * engine: a call is its channel's, `mmcalls:<channel>`, joined on the engine's
 * own socket with this session's token. Whether it runs and is on in a channel,
 * its ICE servers and who is in a call are read here. Mirrors
 * `mattermost/calls.rs` in rv-core.
 */

import type { MattermostCallCredentials, NativeCalls } from '../../lib/call.ts';
import type { MmClient } from './client.ts';
import type { MmDirectory } from './directory.ts';

export const PLUGIN_CALL = 'mmcalls:';
const PLUGIN = '/plugins/com.mattermost.calls';

type Doc = Record<string, unknown>;

export function iceServersOf(list: unknown): MattermostCallCredentials['iceServers'] {
  if (!Array.isArray(list)) return [];
  const text = (v: unknown) => (typeof v === 'string' ? v : '');
  return list.flatMap((raw) => {
    const s = (raw ?? {}) as Doc;
    const urls = typeof s.urls === 'string' ? [s.urls] : Array.isArray(s.urls) ? s.urls.filter((u): u is string => typeof u === 'string') : null;
    return urls === null ? [] : [{ urls, username: text(s.username), credential: text(s.credential) }];
  });
}

export function sessionsOf(state: unknown): [string, string][] {
  const sessions = ((state as Doc | null)?.call as Doc | undefined)?.sessions;
  if (!Array.isArray(sessions)) return [];
  return sessions.flatMap((raw) => {
    const s = (raw ?? {}) as Doc;
    return typeof s.session_id === 'string' && typeof s.user_id === 'string' ? [[s.session_id, s.user_id] as [string, string]] : [];
  });
}

export function pluginCalls(client: MmClient, directory: MmDirectory, userId: string): NativeCalls {
  const get = <T>(path: string) => client.get<T>(`${PLUGIN}${path}`, { root: true, quiet: true });
  let running: boolean | null = null;
  const people = new Map<string, string>();
  /** The channel of the call last joined, whose people `personName` reads. */
  let current: string | null = null;
  const channelOf = (id: string) => (id.startsWith(PLUGIN_CALL) ? id.slice(PLUGIN_CALL.length) : id);
  return {
    available: async (room) => {
      if (running === null) running = await get('/version').then(() => true, () => false);
      if (!running || room === undefined) return running;
      const state = await get<Doc>(`/${encodeURIComponent(room)}`).catch(() => null);
      if (typeof state?.enabled === 'boolean') return state.enabled;
      const config = await get<Doc>('/config').catch(() => null);
      return config?.DefaultEnabled === true;
    },
    memo: () => running === true,
    start: async (room) => `${PLUGIN_CALL}${room}`,
    join: async (id) => (id.startsWith(PLUGIN_CALL) ? id : `${PLUGIN_CALL}${id}`),
    mattermost: async (id) => {
      const config = await get<Doc>('/config').catch(() => ({} as Doc));
      const iceServers = iceServersOf(config.ICEServersConfigs);
      if (config.NeedsTURNCredentials === true) iceServers.push(...iceServersOf(await get<unknown>('/turn-credentials').catch(() => [])));
      if (client.token === null) throw new Error('signed_out');
      current = channelOf(id);
      return { server: client.baseUrl, token: client.token, channelId: channelOf(id), userId, iceServers };
    },
    personName: async (identity) => {
      if (!people.has(identity) && current !== null) {
        for (const [session, user] of sessionsOf(await get<unknown>(`/${encodeURIComponent(current)}`).catch(() => null))) people.set(session, user);
        await directory.ensure([...new Set(people.values())]);
      }
      const user = people.get(identity);
      const known = user === undefined ? null : directory.user(user);
      return known?.displayName ?? known?.username ?? null;
    },
  };
}
