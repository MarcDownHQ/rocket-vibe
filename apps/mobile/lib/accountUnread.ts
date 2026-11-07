/**
 * Whether an account other than the open one has unread messages: the dot on
 * its button in the server rail (`ui/serverRail.tsx`). Pure rules over one
 * cheap read per account, tested under Node; the reads themselves live in the
 * rail, which owns the Keystore.
 */
import type { AccountUnread } from '@rocket-vibe/provider-api/session';
import type { Room } from '../providers/vibez/protocol.generated.ts';
import { readBadges } from '../providers/vibez/readStates.ts';

/** `subscriptions.get` read like the desktop: an open subscription with unread messages or an alert. */
export function subscriptionsUnread(response: unknown): AccountUnread {
  const update = (response as { update?: unknown } | null)?.update;
  if (!Array.isArray(update)) return 'none';
  let state: AccountUnread = 'none';
  for (const raw of update as unknown[]) {
    const s = raw as { t?: unknown; open?: unknown; unread?: unknown; alert?: unknown; userMentions?: unknown } | null;
    if (s === null || typeof s !== 'object' || s.open === false) continue;
    const unread = typeof s.unread === 'number' && s.unread > 0;
    if ((s.t === 'd' && unread) || (typeof s.userMentions === 'number' && s.userMentions > 0)) return 'attention';
    if (unread || s.alert === true) state = 'unread';
  }
  return state;
}
/** A Vibez account's rooms (`GET /api/v1/rooms`): any read state with something to read. */
export function nativeRoomsUnread(rooms: readonly Room[]): AccountUnread {
  let state: AccountUnread = 'none';
  for (const room of rooms) {
    const badges = readBadges(room.read_state ?? null);
    if (badges.mentions > 0 || badges.groupMentions > 0 || (room.kind === 'direct' && badges.unread > 0)) return 'attention';
    if (badges.alert) state = 'unread';
  }
  return state;
}
/** The rail's key for a server: its host, as pushes and deep links carry it. */
export function serverHost(baseUrl: string): string {
  try {
    return new URL(baseUrl.includes('://') ? baseUrl : `https://${baseUrl}`).host.toLowerCase();
  } catch {
    return baseUrl.toLowerCase();
  }
}
