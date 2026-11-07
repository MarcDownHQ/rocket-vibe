export type ThreadScope = 'all' | 'participated' | 'mentioned' | 'others';

export const THREAD_SCOPES: readonly ThreadScope[] = ['all', 'participated', 'mentioned', 'others'];

/** I wrote the root, I replied, or the thread lives in a direct conversation. */
export function threadParticipated(me: string, thread: { rootAuthor: string; repliers: readonly string[]; direct: boolean }): boolean {
  return thread.direct || thread.rootAuthor === me || thread.repliers.includes(me);
}

/** The provider counts a mention of me, or a text the listing returned names me. */
export function threadMentioned(thread: { mentions: number; texts: readonly (string | null)[]; names: (text: string) => boolean }): boolean {
  return thread.mentions > 0 || thread.texts.some((text) => text !== null && thread.names(text));
}

/** `@name` as a whole word of a text; `@all` and `@here` are not me. */
export function writtenMention(username: string | null): (text: string) => boolean {
  if (username === null || username === '') return () => false;
  const escaped = username.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const pattern = new RegExp(`(?:^|[^\\w@.-])@${escaped}(?![\\w-]|\\.[\\w-])`, 'i');
  return (text) => pattern.test(text);
}

export function inThreadScope(entry: { participated: boolean; mentioned: boolean }, scope: ThreadScope): boolean {
  switch (scope) {
    case 'all':
      return true;
    case 'participated':
      return entry.participated;
    case 'mentioned':
      return entry.mentioned;
    case 'others':
      return !entry.participated && !entry.mentioned;
  }
}
