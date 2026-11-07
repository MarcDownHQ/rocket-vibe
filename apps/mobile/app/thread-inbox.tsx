import { Redirect, Stack, useRouter } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Alert, FlatList, StyleSheet, Text, View } from 'react-native';

import { textPreview } from '../lib/markdown.ts';
import type { ProviderActions, ThreadInboxEntry, ThreadInboxFilter, ThreadInboxPage, ThreadParticipant } from '../lib/provider.ts';
import type { RestClient } from '../lib/rest.ts';
import { coalesced, THREAD_REFRESH_MS } from '../lib/threadInbox.ts';
import { avatarUrl } from '../lib/upload.ts';
import { useListTimeFormatter, useT } from '../ui/i18n.ts';
import { Icon } from '../ui/icon.tsx';
import { useAvatarEtags, useDisplayNames } from '../ui/identities.tsx';
import { AvatarTile, UnreadBadge } from '../ui/kit.tsx';
import { useSession } from '../ui/session.tsx';
import { useSync } from '../ui/sync.tsx';
import { Tappable } from '../ui/tappable.tsx';
import { type Colors, FONTS, useColors } from '../ui/theme.ts';
import { useThreadTotals } from '../ui/threadInbox.ts';

/**
 * The account's threads across its rooms: those I follow, or those with replies
 * I have not read. Rendered from the provider like the room's own thread list.
 * A row opens its thread and leaves the thread's room unread.
 */

const AVATARS = 3;

type ListState =
  | { phase: 'loading' }
  | { phase: 'failed' }
  | { phase: 'ready'; entries: ThreadInboxEntry[]; next: string | null; pages: number; more: boolean; moreFailed: boolean };

export default function ThreadInboxScreen() {
  const { state } = useSession();
  const sync = useSync();
  const c = useColors();
  const t = useT();

  if (state.phase === 'disconnected') return <Redirect href="/login" />;

  if (state.phase !== 'connected' || sync.phase !== 'ready') {
    return (
      <View style={[styles.center, { backgroundColor: c.background }]}>
        <Stack.Screen options={{ title: t('threads.title') }} />
        <ActivityIndicator />
      </View>
    );
  }
  return <ThreadInbox c={c} client={state.client} actions={sync.actions} generation={sync.generation} />;
}

function ThreadInbox({
  c,
  client,
  actions,
  generation,
}: {
  c: Colors;
  client: RestClient;
  actions: ProviderActions;
  generation: number;
}) {
  const t = useT();
  const router = useRouter();
  const [filter, setFilter] = useState<ThreadInboxFilter>('followed');
  const [list, setList] = useState<ListState>({ phase: 'loading' });
  const totals = useThreadTotals(actions, generation);
  const request = useRef(0);

  const first = useCallback(() => {
    const asked = ++request.current;
    return (actions.threadInbox?.(filter, null) ?? Promise.resolve<ThreadInboxPage>({ entries: [], next: null })).then(
      (page) => {
        if (asked !== request.current) return;
        setList({ phase: 'ready', entries: page.entries, next: page.next, pages: 1, more: false, moreFailed: false });
      },
      () => {
        if (asked !== request.current) return;
        setList((now) => (now.phase === 'ready' ? now : { phase: 'failed' }));
      },
    );
  }, [actions, filter]);

  const reload = useCallback(() => {
    setList({ phase: 'loading' });
    void first();
  }, [first]);

  useEffect(() => {
    void first();
  }, [first]);

  // A list read further than its first page stays as it is: reading it again
  // would throw the reader back to the top.
  const pages = list.phase === 'ready' ? list.pages : 0;
  const onePage = useRef(true);
  useEffect(() => {
    onePage.current = pages <= 1;
  }, [pages]);
  useEffect(() => {
    const pace = coalesced(() => {
      if (onePage.current) void first();
    }, THREAD_REFRESH_MS);
    const release = actions.onThreadsChanged?.(pace.call);
    return () => {
      pace.cancel();
      release?.();
    };
  }, [actions, first]);

  const lastGeneration = useRef(generation);
  useEffect(() => {
    if (lastGeneration.current === generation) return;
    lastGeneration.current = generation;
    if (onePage.current) void first();
  }, [generation, first]);

  const loadMore = useCallback(
    (retrying = false) => {
      if (list.phase !== 'ready' || list.next === null || list.more || (list.moreFailed && !retrying)) return;
      if (actions.threadInbox === undefined) return;
      const asked = ++request.current;
      setList({ ...list, more: true, moreFailed: false });
      actions.threadInbox(filter, list.next).then(
        (page) => {
          if (asked !== request.current) return;
          setList((now) => {
            if (now.phase !== 'ready') return now;
            // A thread answered meanwhile moves up and shifts the pages.
            const known = new Set(now.entries.map((entry) => entry.root.id));
            const fresh = page.entries.filter((entry) => !known.has(entry.root.id));
            return { ...now, entries: [...now.entries, ...fresh], next: page.next, pages: now.pages + 1, more: false };
          });
        },
        () => {
          if (asked !== request.current) return;
          setList((now) => (now.phase === 'ready' ? { ...now, more: false, moreFailed: true } : now));
        },
      );
    },
    [list, actions, filter],
  );

  const open = useCallback(
    (entry: ThreadInboxEntry) => {
      setList((now) =>
        now.phase === 'ready'
          ? { ...now, entries: now.entries.map((e) => (e === entry ? { ...e, unreadReplies: 0, unreadMentions: 0 } : e)) }
          : now,
      );
      router.push({ pathname: '/thread/[id]', params: { id: entry.root.id, rid: entry.root.rid } });
    },
    [router],
  );

  const [marking, setMarking] = useState(false);
  const markAll = useCallback(() => {
    if (actions.markAllThreadsRead === undefined || marking) return;
    setMarking(true);
    actions
      .markAllThreadsRead()
      .then(first, () => {
        Alert.alert(t('threadInbox.markAllFailed'), undefined, [{ text: t('common.close') }], { cancelable: true });
      })
      .finally(() => setMarking(false));
  }, [actions, marking, first, t]);
  const canMark = (totals?.unreadThreads ?? 0) > 0 && !marking;

  return (
    <View style={[styles.full, { backgroundColor: c.background }]}>
      <Stack.Screen
        options={{
          title: t('threads.title'),
          headerRight: () => (
            <Tappable
              onPress={markAll}
              disabled={!canMark}
              hitSlop={8}
              android_ripple={{ color: c.ripple, borderless: true }}
              accessibilityRole="button"
              accessibilityLabel={t('threadInbox.markAll')}
              accessibilityState={{ disabled: !canMark }}
              style={{ padding: 4, opacity: canMark ? 1 : 0.4 }}
            >
              <Icon name="object-select" size={21} color={c.secondaryText} />
            </Tappable>
          ),
        }}
      />
      <View style={styles.tabs} accessibilityRole="tablist">
        {(['followed', 'unread'] as const).map((option) => {
          const active = option === filter;
          return (
            <Tappable
              key={option}
              onPress={() => {
                if (active) return;
                setList({ phase: 'loading' });
                setFilter(option);
              }}
              accessibilityRole="tab"
              accessibilityState={{ selected: active }}
              android_ripple={{ color: c.ripple, borderless: false }}
              style={[
                styles.tab,
                {
                  borderColor: active ? c.accent : c.border,
                  backgroundColor: active ? c.surfaceActive : 'transparent',
                },
              ]}
            >
              <Text style={[styles.tabText, { color: active ? c.text : c.dimmed }]}>
                {t(option === 'followed' ? 'threadInbox.followed' : 'threadInbox.unread')}
              </Text>
            </Tappable>
          );
        })}
      </View>
      {list.phase === 'loading' ? (
        <View style={styles.center}>
          <ActivityIndicator />
        </View>
      ) : list.phase === 'failed' ? (
        <View style={styles.center}>
          <Text style={[styles.empty, { color: c.errorText }]}>{t('threads.loadFailed')}</Text>
          <Tappable onPress={reload} hitSlop={8}>
            <Text style={[styles.retry, { color: c.accent }]}>{t('common.retry')}</Text>
          </Tappable>
        </View>
      ) : list.entries.length === 0 ? (
        <View style={styles.center}>
          <Text style={[styles.emptyTitle, { color: c.text }]}>
            {t(filter === 'followed' ? 'threadInbox.noneFollowed' : 'threadInbox.noneUnread')}
          </Text>
          {filter === 'unread' && <Text style={[styles.empty, { color: c.dimmed }]}>{t('threadInbox.caughtUp')}</Text>}
        </View>
      ) : (
        <FlatList
          data={list.entries}
          keyExtractor={(entry) => entry.root.id}
          renderItem={({ item }) => <EntryRow c={c} client={client} entry={item} onOpen={open} />}
          onEndReached={() => loadMore()}
          onEndReachedThreshold={0.5}
          ListFooterComponent={
            list.more ? (
              <ActivityIndicator style={styles.more} />
            ) : list.moreFailed ? (
              <Tappable onPress={() => loadMore(true)} hitSlop={8} style={styles.more}>
                <Text style={[styles.retry, styles.centered, { color: c.accent }]}>{t('common.retry')}</Text>
              </Tappable>
            ) : null
          }
          contentContainerStyle={styles.content}
        />
      )}
    </View>
  );
}

function EntryRow({
  c,
  client,
  entry,
  onOpen,
}: {
  c: Colors;
  client: RestClient;
  entry: ThreadInboxEntry;
  onOpen: (entry: ThreadInboxEntry) => void;
}) {
  const t = useT();
  const formatTime = useListTimeFormatter();
  const names = useDisplayNames();
  const { root } = entry;
  const author = names.get(root.authorId) ?? root.authorName ?? '?';
  const preview = textPreview(root.text ?? '').trim();
  const unread = entry.unreadReplies > 0;
  return (
    <Tappable
      onPress={() => onOpen(entry)}
      android_ripple={{ color: c.ripple }}
      accessibilityRole="button"
      style={[styles.row, { borderBottomColor: c.softBorder, backgroundColor: unread ? c.surfaceActive : 'transparent' }]}
    >
      <View style={styles.head}>
        <Text numberOfLines={1} style={[styles.author, { color: c.text }]}>{author}</Text>
        {entry.room !== '' && (
          <View style={[styles.chip, { borderColor: c.border }]}>
            <Text numberOfLines={1} style={[styles.chipText, { color: c.secondaryText }]}>{entry.room}</Text>
          </View>
        )}
        <Text style={[styles.time, { color: c.dimmed }]}>{formatTime(entry.lastReply)}</Text>
      </View>
      <Text numberOfLines={2} style={[styles.preview, { color: c.text }]}>
        {preview === '' ? t('common.attachment') : preview}
      </Text>
      <View style={styles.foot}>
        {entry.participants.slice(0, AVATARS).map((person) => (
          <Participant key={person.id} c={c} client={client} person={person} />
        ))}
        <Text style={[styles.replies, { color: c.dimmed }]}>{t('messageRow.replies', { n: entry.replies })}</Text>
        <View style={styles.badge}>
          <UnreadBadge c={c} n={entry.unreadReplies} mentioned={entry.unreadMentions > 0} />
        </View>
      </View>
    </Tappable>
  );
}

function Participant({ c, client, person }: { c: Colors; client: RestClient; person: ThreadParticipant }) {
  const names = useDisplayNames();
  const etags = useAvatarEtags();
  return (
    <AvatarTile
      c={c}
      hueKey={person.username}
      initial={(names.get(person.id) ?? person.username).charAt(0)}
      size={22}
      radius={8}
      uri={avatarUrl(client, { uid: person.id, username: person.username, etag: etags.byUid.get(person.id) })}
    />
  );
}

const styles = StyleSheet.create({
  full: { flex: 1 },
  center: { flex: 1, alignItems: 'center', justifyContent: 'center', padding: 24, gap: 12 },
  tabs: { flexDirection: 'row', gap: 8, paddingHorizontal: 16, paddingVertical: 12 },
  tab: { borderWidth: 1, borderRadius: 999, paddingHorizontal: 16, paddingVertical: 7, overflow: 'hidden' },
  tabText: { fontFamily: FONTS.bodySemi, fontSize: 13.5 },
  content: { paddingBottom: 24 },
  row: { paddingHorizontal: 16, paddingVertical: 10, gap: 4, borderBottomWidth: 1 },
  head: { flexDirection: 'row', alignItems: 'center', gap: 8 },
  author: { flexShrink: 1, fontFamily: FONTS.bodyBold, fontSize: 14.5 },
  chip: { flexShrink: 1, borderWidth: 1, borderRadius: 6, paddingHorizontal: 6, paddingVertical: 1 },
  chipText: { fontFamily: FONTS.body, fontSize: 12 },
  time: { marginLeft: 'auto', fontFamily: FONTS.body, fontSize: 12 },
  preview: { fontFamily: FONTS.body, fontSize: 14 },
  foot: { flexDirection: 'row', alignItems: 'center', gap: 4, marginTop: 2 },
  replies: { marginLeft: 4, fontFamily: FONTS.bodySemi, fontSize: 12.5 },
  badge: { marginLeft: 'auto' },
  more: { padding: 16 },
  centered: { textAlign: 'center' },
  emptyTitle: { textAlign: 'center', fontFamily: FONTS.bodyBold, fontSize: 16 },
  empty: { textAlign: 'center', fontFamily: FONTS.body, fontSize: 14 },
  retry: { fontFamily: FONTS.bodyBold, fontSize: 14 },
});
