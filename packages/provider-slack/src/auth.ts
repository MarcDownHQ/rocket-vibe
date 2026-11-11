/**
 * Slack sign-in from a browser session: the web client's xoxc token and its
 * `d` cookie, pasted or brought by the pairing handoff. `auth.test` names the
 * workspace and the account; nothing else proves them. Signing out is local:
 * revoking the browser session would also sign the browser out.
 */

import type { AccountUnread, Session } from '@rocket-vibe/provider-api/session';
import { SlackClient, SlackError, validateCredentials, type SlackAccount, type SlackClientOptions } from './client.ts';

export type SlackLoginOptions = { fetch?: typeof fetch };

export async function loginSlack(token: string, cookie: string, expected: SlackAccount | null = null, options: SlackLoginOptions = {}): Promise<Session> {
  const credentials = { token: token.trim(), cookie: cookie.trim().replace(/^d=/, '') };
  validateCredentials(credentials.token, credentials.cookie);
  const client = new SlackClient(credentials.token, credentials.cookie, { fetch: options.fetch, expected });
  try {
    const identity = await client.authenticate();
    return {
      baseUrl: identity.origin,
      authToken: credentials.token,
      userId: identity.userId,
      username: identity.user,
      kind: 'slack',
      siteUrl: null,
      slackCookie: credentials.cookie,
      slackTeamId: identity.teamId,
    };
  } finally {
    client.close();
  }
}

/** The account's credentials and its pinned identity; a record without them cannot work. */
export function slackClientFor(session: Session, options: Omit<SlackClientOptions, 'expected'> = {}): SlackClient {
  if (session.kind !== 'slack' || session.slackCookie === undefined || session.slackTeamId === undefined) throw new SlackError('invalid_credentials');
  return new SlackClient(session.authToken, session.slackCookie, { ...options, expected: { teamId: session.slackTeamId, userId: session.userId } });
}

export async function resumeSlack(session: Session, options: SlackLoginOptions = {}): Promise<Session> {
  const client = slackClientFor(session, { fetch: options.fetch });
  try {
    const identity = await client.authenticate();
    return { ...session, username: identity.user };
  } finally {
    client.close();
  }
}

/** Does an account that is not open have something unread? One `client.counts`, the session never refused from here. */
export async function slackUnread(session: Session, options: SlackLoginOptions = {}): Promise<AccountUnread> {
  const client = slackClientFor(session, { fetch: options.fetch });
  try {
    const body = await client.call('client.counts');
    let state: AccountUnread = 'none';
    for (const list of ['channels', 'mpims', 'ims'] as const) {
      const entries = body[list];
      for (const raw of Array.isArray(entries) ? entries : []) {
        if (typeof raw !== 'object' || raw === null) continue;
        const entry = raw as Record<string, unknown>;
        if (Number(entry.mention_count) > 0 || (list !== 'channels' && entry.has_unreads === true)) return 'attention';
        if (entry.has_unreads === true) state = 'unread';
      }
    }
    return state;
  } finally {
    client.close();
  }
}
