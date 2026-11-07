import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Alert, StyleSheet, Text } from 'react-native';

import { followedBy, followersAfter } from '../lib/marks.ts';
import type { ProviderActions } from '../lib/provider.ts';
import type { SyncEngine } from '../lib/sync.ts';
import { coalesced, THREAD_REFRESH_MS } from '../lib/threadInbox.ts';
import { useT } from './i18n.ts';
import { Tappable } from './tappable.tsx';
import { InlineIcon } from './icon.tsx';
import { FONTS, type Colors } from './theme.ts';

/**
 * Following a thread (Rocket.Chat `chat.followMessage`). The state is read from
 * the root's followers (`thread_followers`), or from the server where the
 * provider asks it (`threadFollowing`, Mattermost); a gesture shows its outcome
 * at once and writes the column after success, the server's rebroadcast of the
 * root confirming it a moment later. `null` where the provider cannot follow.
 */
export function useThreadFollow({
  actions,
  engine,
  rid,
  root,
  followers,
  myId,
}: {
  actions: ProviderActions;
  engine: SyncEngine;
  rid: string | null;
  root: string;
  followers: string | null | undefined;
  myId: string;
}): { following: boolean; busy: boolean; toggle: () => void } | null {
  const t = useT();
  // The gesture's outcome until the column says the same.
  const [pending, setPending] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const asked = useThreadFollowing(actions, rid, root);
  const stored = asked === undefined ? followedBy(followers ?? null, myId) : asked.following === true;
  const following = pending ?? stored;
  // Once the column agrees, it rules again: a change made elsewhere then shows.
  useEffect(() => {
    if (pending !== null && !busy && stored === pending) setPending(null);
  }, [pending, busy, stored]);
  const follow = actions.followThread?.bind(actions);
  const toggle = useCallback(() => {
    if (follow === undefined || rid === null || busy) return;
    const put = !following;
    setPending(put);
    setBusy(true);
    void (async () => {
      if (asked !== undefined) {
        try {
          await follow(rid, root, put);
          asked.set(put);
        } catch {
          setPending(null);
          Alert.alert(t('threads.followFailed'), undefined, [{ text: t('common.close') }], {
            cancelable: true,
          });
        }
        setBusy(false);
        return;
      }
      try {
        await follow(rid, root, put);
      } catch {
        setPending(null);
        setBusy(false);
        Alert.alert(t('threads.followFailed'), undefined, [{ text: t('common.close') }], {
          cancelable: true,
        });
        return;
      }
      try {
        await engine.syncStore.updateThreadFollowers(root, followersAfter(followers ?? null, myId, put));
      } catch {
        // The root the server rebroadcasts writes the same column.
      }
      setBusy(false);
    })();
  }, [follow, rid, busy, following, root, engine, followers, myId, t, asked]);
  if (follow === undefined || asked?.failed === true) return null;
  return { following, busy: busy || asked?.following === null, toggle };
}

type AskedFollowing = { following: boolean | null; failed: boolean; set: (following: boolean) => void };

/**
 * Whether I follow the thread, from the server where the root has no follower
 * list; `undefined` where it has one. `following` is null until it answered,
 * and `failed` when it could not: the bell must not then guess.
 */
function useThreadFollowing(actions: ProviderActions, rid: string | null, root: string): AskedFollowing | undefined {
  const [following, setFollowing] = useState<boolean | null>(null);
  const [failed, setFailed] = useState(false);
  const asking = useRef(0);
  const known = useRef(false);
  const read = actions.threadFollowing !== undefined;
  useEffect(() => {
    if (!read || rid === null) return;
    const ask = () => {
      const question = ++asking.current;
      actions.threadFollowing!(rid, root).then(
        (answer) => {
          if (question !== asking.current) return;
          known.current = true;
          setFollowing(answer);
          setFailed(false);
        },
        () => {
          if (question === asking.current && !known.current) setFailed(true);
        },
      );
    };
    ask();
    const pace = coalesced(ask, THREAD_REFRESH_MS);
    const release = actions.onThreadsChanged?.(pace.call);
    return () => {
      asking.current++;
      pace.cancel();
      release?.();
    };
  }, [actions, read, rid, root]);
  const set = useCallback((value: boolean) => {
    asking.current++;
    known.current = true;
    setFollowing(value);
    setFailed(false);
  }, []);
  return useMemo(() => (read ? { following, failed, set } : undefined), [read, following, failed, set]);
}

/** The bell of a thread: lit while I follow it. */
export function FollowButton({
  c,
  following,
  busy,
  onPress,
  compact = false,
}: {
  c: Colors;
  following: boolean;
  busy: boolean;
  onPress: () => void;
  /** The list's rows: the bell alone, the label for screen readers only. */
  compact?: boolean;
}) {
  const t = useT();
  const label = t(following ? 'threads.unfollow' : 'threads.follow');
  return (
    <Tappable
      onPress={onPress}
      disabled={busy}
      hitSlop={8}
      android_ripple={{ color: c.ripple, borderless: true }}
      accessibilityRole="switch"
      accessibilityState={{ checked: following, busy }}
      accessibilityLabel={t('threads.following')}
      accessibilityHint={label}
      style={({ pressed }) => [styles.button, { opacity: pressed || busy ? 0.5 : 1 }]}
    >
      <Text style={[styles.text, { color: following ? c.accent : c.dimmed }]}>
        <InlineIcon name={following ? 'preferences-system-notifications' : 'notifications-disabled'} />
        {!compact && ` ${t(following ? 'threads.followingShort' : 'threads.follow')}`}
      </Text>
    </Tappable>
  );
}

const styles = StyleSheet.create({
  button: { paddingHorizontal: 6, paddingVertical: 4 },
  text: { fontFamily: FONTS.bodySemi, fontSize: 13.5 },
});
