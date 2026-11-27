/**
 * A room's voice session (docs/protocol/VOICE.md): one tile per connected
 * person, sharing all the screen, whose border lights up while they speak; a
 * shared screen on a stage that goes full screen at a tap; the chat one tap
 * away, and the microphone / sound / leave controls. A long press on someone
 * sets their volume here. A direct call over gives the chat back.
 */
import { Stack, useLocalSearchParams, useRouter } from 'expo-router';
import { useEffect, useRef, useState } from 'react';
import { Pressable, StyleSheet, Text, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { TILE_GAP, arrangeTiles } from '../../lib/voiceGrid.ts';
import { useT } from '../../ui/i18n.ts';
import { useSession } from '../../ui/session.tsx';
import { FONTS, useColors } from '../../ui/theme.ts';
import { Icon, InlineIcon } from '../../ui/icon.tsx';
import { VoiceReactions } from '../../ui/social.tsx';
import { ScreenStage, VoiceControls, VoiceTile, type VoiceCard, useJoinVoice, useListening, usePeople, usePersonSheet, useRoomVoice, useVoice } from '../../ui/voice.tsx';
import { VoiceVideoView } from '../../modules/voice/index.ts';

type Card = VoiceCard;

export default function VoiceScreen() {
  const { rid, title, direct: directParam } = useLocalSearchParams<{ rid: string; title?: string; direct?: string }>();
  const c = useColors();
  const t = useT();
  const router = useRouter();
  const { state } = useSession();
  const voice = useVoice();
  const occupants = useRoomVoice(rid);
  const people = usePeople();
  const listening = useListening();
  const openPerson = usePersonSheet();
  const join = useJoinVoice();
  const here = voice.room === rid && voice.phase !== 'idle';
  const direct = directParam === '1' || (voice.room === rid && voice.direct);
  const [area, setArea] = useState({ width: 0, height: 0 });

  // A direct call over (hung up here or there): back to its chat.
  const wasHere = useRef(false);
  useEffect(() => {
    if (wasHere.current && !here && direct) router.dismissTo({ pathname: '/room/[rid]', params: { rid } });
    wasHere.current = here;
  }, [here, direct, router, rid]);

  const mutedHere = (uid: string): boolean => listening.people[uid]?.muted === true;
  // Connected here: LiveKit's own view (who speaks, now). Elsewhere: the server's.
  const cards: Card[] = here
    ? voice.participants.map(p => ({
        uid: p.identity, name: people(p.identity).name, speaking: p.speaking, muted: p.muted, deafened: p.deafened,
        local: p.local, camera: p.camera === true, screen: p.screen === true, mutedHere: mutedHere(p.identity),
      }))
    : occupants.map(o => ({
        uid: o.user.id, name: o.user.display_name || o.user.username, speaking: false, muted: o.muted,
        deafened: o.deafened, local: false, camera: false, screen: false, mutedHere: mutedHere(o.user.id),
      }));
  // The room's one screen share takes most of the screen.
  const sharer = here ? cards.find(card => card.screen) : undefined;
  const status = !here ? null
    : voice.ring?.state === 'ringing' && voice.participants.length < 2 ? t('voice.ringing')
    : voice.phase === 'connected' ? t('voice.connected')
    : voice.phase === 'reconnecting' ? t('voice.reconnecting') : t('voice.connecting');
  const press = (card: Card) => card.local ? undefined : () => openPerson(card.uid, card.name);

  if (state.phase !== 'connected') return null;
  const layout = arrangeTiles(cards.length, area.width, area.height);
  const rows: Card[][] = [];
  for (let i = 0; i < cards.length; i += layout.columns) rows.push(cards.slice(i, i + layout.columns));
  return (
    <SafeAreaView style={[styles.full, { backgroundColor: c.background }]} edges={['top', 'bottom']}>
      <Stack.Screen options={{ headerShown: false }} />
      <View style={[styles.header, { borderBottomColor: c.softBorder }]}>
        <Pressable onPress={() => router.back()} hitSlop={10} accessibilityRole="button" accessibilityLabel={t('room.back')}>
          <Text style={[styles.back, { color: c.purple }]}>‹</Text>
        </Pressable>
        <View style={styles.headerText}>
          <Text style={[styles.title, { color: c.text }]} numberOfLines={1}><InlineIcon name="audio-volume-high" /> {title ?? ''}</Text>
          {status !== null && (
            <Text style={[styles.status, { color: voice.phase === 'connected' ? c.online : c.dimmed }]} numberOfLines={1}>
              {voice.encrypted ? <><InlineIcon name="channel-secure" /> {status} · {t('voice.secure')}</> : status}
            </Text>
          )}
        </View>
        <Pressable
          onPress={() => router.push({ pathname: '/room/[rid]', params: { rid } })}
          hitSlop={8}
          accessibilityRole="button"
          accessibilityLabel={t('voice.openChat')}
          android_ripple={{ color: c.ripple, borderless: true }}
        >
          <Icon name="chat-message-new" size={22} color={c.secondaryText} />
        </Pressable>
      </View>
      {sharer !== undefined && VoiceVideoView !== null ? (
        <ScreenStage c={c} client={state.client} sharer={sharer} cards={cards} press={press} />
      ) : cards.length === 0 ? (
        <Text style={[styles.empty, { color: c.dimmed }]}>{t('voice.empty')}</Text>
      ) : (
        <View style={styles.grid} onLayout={e => setArea({ width: e.nativeEvent.layout.width, height: e.nativeEvent.layout.height })}>
          {area.width > 0 && rows.map(row => (
            <View key={row[0]!.uid} style={styles.gridRow}>
              {row.map(card => (
                <VoiceTile key={card.uid} c={c} client={state.client} card={card} width={layout.width} height={layout.height}
                  you={t('voice.you')} onLongPress={press(card)} />
              ))}
            </View>
          ))}
        </View>
      )}
      <View style={[styles.footer, { borderTopColor: c.softBorder }]}>
        {here ? (
          <><VoiceReactions room={rid}/>
          <VoiceControls c={c} size="large" /></>
        ) : (
          <Pressable
            onPress={() => void join(rid, title ?? '', false, direct)}
            accessibilityRole="button"
            style={({ pressed }) => [styles.join, { backgroundColor: c.online, opacity: pressed ? 0.7 : 1 }]}
          >
            <Text style={[styles.joinText, { color: c.background }]}>{t('voice.join')}</Text>
          </Pressable>
        )}
      </View>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  full: { flex: 1 },
  header: { flexDirection: 'row', alignItems: 'center', gap: 12, paddingHorizontal: 14, paddingVertical: 10, borderBottomWidth: 1 },
  back: { fontSize: 30, lineHeight: 30, paddingHorizontal: 4 },
  headerText: { flex: 1 },
  title: { fontFamily: FONTS.titleStrong, fontSize: 17 },
  status: { fontFamily: FONTS.bodyBold, fontSize: 12 },
  grid: { flex: 1, margin: 12, gap: TILE_GAP, justifyContent: 'center' },
  gridRow: { flexDirection: 'row', justifyContent: 'center', gap: TILE_GAP },
  empty: { flex: 1, textAlign: 'center', padding: 32, fontFamily: FONTS.body, fontSize: 14 },
  footer: { paddingVertical: 14, paddingHorizontal: 16, borderTopWidth: 1 },
  join: { alignSelf: 'center', borderRadius: 24, paddingHorizontal: 28, paddingVertical: 12 },
  joinText: { fontFamily: FONTS.bodyStrong, fontSize: 15 },
});
