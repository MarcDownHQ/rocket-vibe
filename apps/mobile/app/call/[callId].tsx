import { Redirect, Stack, useLocalSearchParams, useRouter } from 'expo-router';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  ActivityIndicator,
  Linking,
  PermissionsAndroid,
  Platform,
  Pressable,
  StyleSheet,
  Text,
  View,
} from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { WebView } from 'react-native-webview';

import { callAllowed as teamsCallAllowed } from '@rocket-vibe/provider-teams/translate';
import type { ExternalCallEngine, PcmCallEngine } from '@rocket-vibe/provider-api/calls';
import { answerableNative, answerNative, callContext, callPersonName, callRoom, callSends, carriesPcm, carryNative, joinConference, joinsInRoomNative, nativeChime, nativeJitsi, nativeMattermost, placeNative } from '../../lib/call.ts';
import { sameOrigin, originOf } from '../../lib/origin.ts';
import { VoiceNative, VoiceVideoView, type VoiceSnapshot } from '../../modules/voice/index.ts';
import type { RestClient } from '../../lib/rest.ts';
import { TILE_GAP, arrangeTiles } from '../../lib/voiceGrid.ts';
import { useT } from '../../ui/i18n.ts';
import type { TranslateFn as T } from '../../ui/messages.ts';
import { ControlButton, ScreenStage, VoiceTile, type VoiceCard } from '../../ui/voice.tsx';
import { notify } from '../../ui/toast.tsx';
import { Icon } from '../../ui/icon.tsx';
import { useSession } from '../../ui/session.tsx';
import {useSync} from '../../ui/sync.tsx';
import { type Colors, FONTS, useColors } from '../../ui/theme.ts';

/**
 * Call screen: the Jitsi conference in a full-screen WebView.
 *
 * Jitsi is a web app: we load the private URL returned by the provider (JWT
 * included) rather than embedding the native SDK (peer RN ~0.79 vs 0.86,
 * fragile New Arch). The WebView stays STRICTLY confined to the call:
 * elsewhere it is forbidden (ROADMAP §4.2) and the app renders everything
 * natively.
 *
 * Camera/mic: on `getUserMedia`, `react-native-webview` maps the web request
 * to the Android permissions (`CAMERA`, `RECORD_AUDIO`) and grants them if the
 * app holds them, otherwise triggers the system dialog. We therefore request
 * them UPFRONT (below) so the prompt appears before the call screen, not in
 * the middle of it.
 */

async function requestMicrophone(): Promise<void> {
  if (Platform.OS !== 'android') return;
  try {
    await PermissionsAndroid.request(PermissionsAndroid.PERMISSIONS.RECORD_AUDIO);
  } catch {
    // A refusal shows as a call nobody hears: the system settings can still grant it.
  }
}

async function requestCameraMic(): Promise<void> {
  if (Platform.OS !== 'android') return;
  try {
    await PermissionsAndroid.requestMultiple([
      PermissionsAndroid.PERMISSIONS.CAMERA,
      PermissionsAndroid.PERMISSIONS.RECORD_AUDIO,
    ]);
  } catch {
    // A refusal does not prevent opening the call: Jitsi will show "no
    // camera", and the WebView will ask again on the first getUserMedia.
  }
}

/**
 * On mobile, `meet.jit.si` shows an "open in the app" interstitial: tapping
 * it tries an `intent://` link to the Jitsi app (not installed) and the
 * WebView fails with `ERR_UNKNOWN_URL_SCHEME`. This Jitsi config flag, passed
 * in the URL hash, removes the interstitial: the conference loads straight
 * in the WebView. A self-hosted Jitsi without deep links ignores it.
 */
function withoutJitsiInterstitial(url: string): string {
  if (url.includes('disableDeepLinking')) return url;
  const flag = 'config.disableDeepLinking=true';
  return url.includes('#') ? `${url}&${flag}` : `${url}#${flag}`;
}

/**
 * User agent of an ORDINARY mobile Chrome, without the "; wv" marker a
 * WebView adds: Jitsi's browser detection (and that of many sites) restricts
 * WebViews identified as such. So we present ourselves as a supported
 * browser.
 */
const UA_MOBILE =
  'Mozilla/5.0 (Linux; Android 14; Pixel) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Mobile Safari/537.36';

/** A desktop Chrome: Teams' web app sends a phone's browser to its app store page. */
const UA_DESKTOP =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36';

/**
 * Scheme + authority of the conference URL: the LOCK of this WebView.
 *
 * The app holds `CAMERA` and `RECORD_AUDIO` during the call (it just asked
 * for them), and `react-native-webview` then answers `onPermissionRequest`
 * WITHOUT a prompt, whatever the page's origin: Android does not let us
 * arbitrate by origin. The only lever left is NAVIGATION. It used to be
 * filtered by scheme only (`^(https?|about|blob|data):`) with an
 * `originWhitelist={['*']}`: any redirect to an arbitrary https was
 * followed, and that page silently opened camera and mic.
 *
 * So we keep only the origin the SERVER designated
 * (`video-conference.join`), plus `about:blank`, which the WebView loads
 * itself between two pages. A conference URL whose origin cannot be read is
 * not loaded at all: that is the right failure.
 *
 * `originOf`/`sameOrigin` come from `lib/origin.ts`: the same primitives as
 * the token guard, with the reason not to use `new URL().origin`.
 */

export default function CallScreen() {
  const c = useColors();
  const { state } = useSession();
  useSync();
  const { callId, title,rid,adhesion,account } = useLocalSearchParams<{ callId: string; title?: string;rid?:string;adhesion?:string;account?:string }>();
  const t = useT();
  // Reached from a logged-in room; a logged-out state (expired session)
  // sends back to login rather than crashing on `client`.
  if (state.phase !== 'connected') return <Redirect href="/login" />;
  const current=callContext(state.client);
  if(account!==undefined&&account!==current)return <Redirect href="/" />;
  return <Call key={`${current}#${callId}#${rid??''}#${adhesion??''}`} c={c} client={state.client} callId={callId} rid={rid} membership={adhesion} title={title ?? t('call.videoCall')} me={state.session.username} />;
}

function Call({
  c,
  client,
  callId,
  rid,
  membership,
  title,
  me,
}: {
  c: Colors;
  client: RestClient;
  callId: string;
  rid?:string;
  membership?:string;
  title: string;
  me: string;
}) {
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const t = useT();
  const [url, setUrl] = useState<string | null>(null);
  // A Jitsi or kMeet call the voice engine carries itself, audio only; the page is the fallback.
  const [native, setNative] = useState(false);
  // Set once the native join failed: the meeting's page from then on.
  const [pageOnly, setPageOnly] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Incremented by "Retry": reruns the loading effect without duplicating its
  // logic in a handler.
  const [attempt, setAttempt] = useState(0);

  // The `join` (provider URL) and the permission request live INSIDE the
  // effect: `setState` only arrives there after an `await`, never synchronously
  // (otherwise render cascades). `alive` neutralises a response arriving after
  // unmount.
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        // A Slack huddle runs natively, else in Slack's app or the browser;
        // Teams' calls the voice engine does not carry stay here, on Teams' web app.
        if (carriesPcm(client)) {
          if (await carriedNatively()) {
            if (alive) setNative(true);
          } else if (alive) setError(t('call.joinFailed'));
          return;
        }
        if (callId.startsWith('mmcalls:')) {
          if (await joinedPlugin()) {
            if (alive) setNative(true);
          } else if (alive) setError(t('call.joinFailed'));
          return;
        }
        if (rid !== undefined && answerableNative(client, rid) && await answeredNatively(rid)) {
          if (alive) setNative(true);
          return;
        }
        if (!alive) return;
        const external = client.kind === 'slack';
        if (!external) await requestCameraMic();
        const u = await joinConference(client, callId,undefined,{room:rid,membership,alive:()=>alive});
        if (!alive) return;
        if (external) {
          if (await joinedHuddle()) {
            if (alive) setNative(true);
            return;
          }
          if (!alive) return;
          await Linking.openURL(u);
          router.back();
        } else if (await placedNatively(u)) {
          if (alive) setNative(true);
        } else if (await joinedNatively(u)) {
          if (alive) setNative(true);
        } else if (originOf(u) === null) {
          // A conference URL without a readable origin (exotic scheme, truncated
          // response) would give no lock to put on the WebView: we refuse rather
          // than load without a guard.
          setError(t('call.joinFailed'));
        } else setUrl(u);
      } catch {
        if (alive) setError(t('call.joinFailed'));
      }
    })();
    // A Teams call started here (its call link, not a Join), or any call of a
    // channel or a meeting chat, runs on the voice engine.
    async function placedNatively(u: string): Promise<boolean> {
      const voice = VoiceNative;
      if (pageOnly || voice === null || rid === undefined) return false;
      if (!u.startsWith('https://teams.microsoft.com/l/call/') && !joinsInRoomNative(client, rid)) return false;
      try {
        await requestMicrophone();
        // Listening before the session opens: the offer can come at once.
        const offered = new Promise<string>((resolve, reject) => {
          const timer = setTimeout(() => { sub.remove(); reject(new Error('no_offer')); }, 20_000);
          const sub = voice.addListener('localOffer', ({ sdp }) => { clearTimeout(timer); sub.remove(); resolve(sdp); });
        });
        const engine: ExternalCallEngine = {
          start: async (identity, options) => {
            if (!await voice.connectExternal(callId, identity, title, false, true, options?.dataChannel ?? null, options?.receiveVideos ?? 0, options?.videoUpFront === true)) throw new Error('voice_unavailable');
          },
          offer: () => offered,
          answer: (sdp) => voice.remoteAnswer(sdp),
          answerOffer: (sdp) => voice.remoteOffer(sdp),
          roster: (members) => voice.remoteRoster(members),
          ended: (reason) => voice.remoteEnded(reason),
          left: () => new Promise((resolve) => {
            const sub = voice.addListener('change', (next) => {
              if (next.state === 'disconnected') { sub.remove(); resolve(); }
            });
          }),
          ...externalVideo(voice),
        };
        return await placeNative(client, rid, engine);
      } catch {
        await voice.disconnect().catch(() => {});
        return false;
      }
    }
    // A WhatsApp call: the account's engine carries it, the voice engine its audio; no page.
    async function carriedNatively(): Promise<boolean> {
      const voice = VoiceNative;
      if (voice === null) return false;
      try {
        await requestMicrophone();
        const engine: PcmCallEngine = {
          connect: async (address) => {
            if (!await voice.connectPcm(callId, address, title)) throw new Error('voice_unavailable');
          },
          roster: (members) => voice.remoteRoster(members),
          ended: (reason) => voice.remoteEnded(reason),
          left: () => new Promise((resolve) => {
            const sub = voice.addListener('change', (next) => {
              if (next.state === 'disconnected') { sub.remove(); resolve(); }
            });
          }),
          microphone: (listener) => {
            let last: boolean | null = null;
            const sub = voice.addListener('change', (next) => {
              const on = next.microphone !== false;
              if (on !== last) { last = on; listener(on); }
            });
            return () => sub.remove();
          },
        };
        return await carryNative(client, callId, engine);
      } catch {
        await voice.disconnect().catch(() => {});
        return false;
      }
    }
    // A Mattermost Calls plugin call runs on the voice engine only: it has no page.
    async function joinedPlugin(): Promise<boolean> {
      if (VoiceNative === null) return false;
      try {
        await requestMicrophone();
        const call = await nativeMattermost(client, callId);
        if (call === null || !alive) return false;
        return await VoiceNative.connectMattermost(callId, call.server, call.token, call.channelId, call.userId, JSON.stringify(call.iceServers), title, { screen: call.screen, camera: call.camera });
      } catch {
        return false;
      }
    }
    // A Teams call ringing here, answered on the voice engine; its page otherwise.
    async function answeredNatively(room: string): Promise<boolean> {
      const voice = VoiceNative;
      if (pageOnly || voice === null) return false;
      try {
        await requestMicrophone();
        const engine: ExternalCallEngine = {
          start: async (identity, options) => {
            if (!await voice.connectExternal(callId, identity, title, true, true, options?.dataChannel ?? null, options?.receiveVideos ?? 0, options?.videoUpFront === true)) throw new Error('voice_unavailable');
          },
          offer: () => Promise.reject(new Error('answering')),
          answer: (sdp) => voice.remoteAnswer(sdp),
          answerOffer: (sdp) => voice.remoteOffer(sdp),
          roster: (members) => voice.remoteRoster(members),
          ended: (reason) => voice.remoteEnded(reason),
          left: () => new Promise((resolve) => {
            const sub = voice.addListener('change', (next) => {
              if (next.state === 'disconnected') { sub.remove(); resolve(); }
            });
          }),
          ...externalVideo(voice),
        };
        return await answerNative(client, room, engine);
      } catch {
        await voice.disconnect().catch(() => {});
        return false;
      }
    }
    async function joinedHuddle(): Promise<boolean> {
      if (pageOnly || VoiceNative === null) return false;
      try {
        await requestMicrophone();
        const chime = await nativeChime(client, callId);
        if (chime === null || !alive) return false;
        return await VoiceNative.connectChime(callId, chime.signalingUrl, chime.audioHost, chime.attendeeId, chime.joinToken, chime.origin, title);
      } catch {
        return false;
      }
    }
    async function joinedNatively(u: string): Promise<boolean> {
      const jitsi = nativeJitsi(client.kind, u);
      if (pageOnly || jitsi === null || VoiceNative === null) return false;
      try {
        return await VoiceNative.connectJitsi(callId, jitsi.meeting, me, jitsi.token, title);
      } catch {
        return false;
      }
    }
    return () => {
      alive = false;
    };
  }, [client, callId, rid,membership,attempt, t, router, me, title, pageOnly]);

  // Handler (outside the effect): resetting the state there is legitimate.
  const retry = useCallback(() => {
    setError(null);
    setUrl(null);
    setAttempt((n) => n + 1);
  }, []);

  const finish = useCallback(() => router.back(), [router]);

  // The native call's state; a join that fails opens the meeting's page instead.
  const [snapshot, setSnapshot] = useState<VoiceSnapshot | null>(null);
  const connectedOnce = useRef(false);
  useEffect(() => {
    if (!native || VoiceNative === null) return;
    const voice = VoiceNative;
    const sub = voice.addListener('change', (next) => {
      setSnapshot(next);
      if (next.state === 'connected') connectedOnce.current = true;
      if (next.state !== 'disconnected') return;
      if (!connectedOnce.current && (next.reason === 'connect_failed' || next.reason === 'connection_failed')) {
        // Never joined: the meeting's page instead.
        setNative(false);
        setPageOnly(true);
      } else if (next.reason === 'client_initiated') finish();
      // Lost for good: the reason stays on screen until closed.
    });
    return () => {
      sub.remove();
      void voice.disconnect();
    };
  }, [native, finish]);

  // Without a foreground service, Android silences the page's microphone and
  // freezes the process once the app is behind or the phone locked.
  useEffect(() => {
    if (url === null || VoiceNative === null) return;
    void VoiceNative.holdCall(title);
    const sub = VoiceNative.addListener('hangUp', finish);
    return () => {
      sub.remove();
      void VoiceNative?.releaseCall();
    };
  }, [url, title, finish]);

  // Non-null as soon as `url` is: the effect above refuses a URL whose origin
  // cannot be read. The render checks again anyway: it is the lock.
  const origin = useMemo(() => (url === null ? null : originOf(url)), [url]);
  // Teams passes through Microsoft's sign-in and CDNs: its lock is Microsoft's
  // own pages (`callAllowed` in provider-teams), not one origin.
  const teams = client.kind === 'teams';
  // The call's room, its chat opened over this screen: the voice engine holds the call meanwhile, back returns here.
  const room = callRoom(callId, rid);

  return (
    <View style={[styles.full, { backgroundColor: '#000', paddingTop: insets.top }]}>
      <Stack.Screen options={{ headerShown: false }} />

      <View style={[styles.bar, { borderBottomColor: c.softBorder }]}>
        <Text style={[styles.title, { color: c.text }]} numberOfLines={1}>
          {title}
        </Text>
        {native && room !== null && (
          <Pressable
            onPress={() => router.push({ pathname: '/room/[rid]', params: { rid: room } })}
            hitSlop={8}
            accessibilityRole="button"
            accessibilityLabel={t('voice.openChat')}
            android_ripple={{ color: c.ripple, borderless: true }}
            style={styles.chat}
          >
            <Icon name="chat-message-new" size={22} color={c.secondaryText} />
          </Pressable>
        )}
        <Pressable
          onPress={finish}
          hitSlop={10}
          accessibilityRole="button"
          accessibilityLabel={t('call.endCall')}
          style={({ pressed }) => [
            styles.finish,
            { backgroundColor: c.errorCard, opacity: pressed ? 0.7 : 1 },
          ]}
        >
          <Text style={[styles.finishText, { color: c.errorText }]}>{t('call.end')}</Text>
        </Pressable>
      </View>

      {native ? (
        <NativeCall c={c} client={client} snapshot={snapshot ?? VoiceNative?.snapshot() ?? null} onClose={finish} />
      ) : error !== null ? (
        <View style={styles.center}>
          <Text style={[styles.errorMessage, { color: c.errorText }]}>{error}</Text>
          <Pressable onPress={retry} style={styles.retry}>
            <Text style={[styles.retryText, { color: c.cyan }]}>{t('common.retry')}</Text>
          </Pressable>
        </View>
      ) : url === null || origin === null ? (
        <View style={styles.center}>
          <ActivityIndicator color={c.accent} size="large" />
          <Text style={[styles.loadingText, { color: c.dimmed }]}>{t('call.connecting')}</Text>
        </View>
      ) : (
        <WebView
          source={{ uri: teams ? url : withoutJitsiInterstitial(url) }}
          style={styles.full}
          userAgent={teams ? UA_DESKTOP : UA_MOBILE}
          // Jitsi starts audio/video without an explicit user gesture.
          mediaPlaybackRequiresUserAction={false}
          allowsInlineMediaPlayback
          // iOS: grants camera/mic without asking each time (no-op on Android).
          mediaCapturePermissionGrantType="grant"
          domStorageEnabled
          originWhitelist={teams ? ['https://*', 'about:*', 'blob:*'] : [origin]}
          // A Jitsi link with target=_blank stays in the WebView instead of opening
          // a ghost window we would never see.
          setSupportMultipleWindows={false}
          // Black background + spinner while loading: no white flash.
          startInLoadingState
          renderLoading={() => (
            <View style={styles.veil}>
              <ActivityIndicator color={c.accent} size="large" />
            </View>
          )}
          // Only lets navigation happen on the conference's origin. An app link
          // (intent://, org.jitsi.meet://) would crash with ERR_UNKNOWN_URL_SCHEME,
          // and any https would inherit camera and mic without a prompt; see
          // `originOf` at the top of the file.
          onShouldStartLoadWithRequest={(req) =>
            teams ? teamsCallAllowed(req.url) : req.url === 'about:blank' || sameOrigin(req.url, origin)
          }
          onNavigationStateChange={(nav) => {
            // Hanging up sends Jitsi to a "close" page: we hand control back to the
            // room. The "End" button remains the guaranteed exit.
            if (/\/close\d*(\.html)?/.test(nav.url)) finish();
          }}
          onError={() => setError(t('call.loadFailed'))}
        />
      )}
    </View>
  );
}

/** Why a native call ended, in words. */
function endReason(t: T, reason: string | null | undefined): string {
  switch (reason) {
    case 'connection_lost': case 'connection_closed': return t('call.endConnectionLost');
    case 'connection_failed': return t('call.endMediaFailed');
    case 'terminated': case 'transport_replaced': return t('call.endServer');
    case 'meeting_ended': return t('call.endServer');
    case 'connect_failed': case 'refused': case 'at_capacity': return t('call.endJoinFailed');
    case 'remote_ended': return t('call.endRemote');
    case 'no_answer': return t('call.endNoAnswer');
    case 'duplicate_identity': return t('call.endElsewhere');
    case 'removed': return t('call.endRemoved');
    default: return reason ? reason : t('call.endServer');
  }
}

/** The own camera on or off; it asks for the camera first. */
async function toggleCamera(on: boolean, t: T): Promise<void> {
  if (on) return VoiceNative?.setCamera(false);
  if (Platform.OS === 'android' && await PermissionsAndroid.request(PermissionsAndroid.PERMISSIONS.CAMERA) !== PermissionsAndroid.RESULTS.GRANTED) {
    notify(t('voice.cameraDenied'));
    return;
  }
  await VoiceNative?.setCamera(true);
}

/** The voice engine's video, for a call whose signalling the app speaks (Teams): its later offers, what this side sends, whose the other side's videos are, and its data channel. */
function externalVideo(voice: NonNullable<typeof VoiceNative>): Pick<ExternalCallEngine, 'offers' | 'sending' | 'sent' | 'videos' | 'sendData' | 'data'> {
  return {
    offers: (listener) => {
      const sub = voice.addListener('localOffer', ({ sdp }) => listener(sdp));
      return () => sub.remove();
    },
    sending: () => {
      const now = voice.snapshot();
      return { camera: now.camera === true, screen: now.sharing === true };
    },
    sent: (listener) => {
      let last = '';
      const sub = voice.addListener('change', (next) => {
        const local = next.participants.find((p) => p.local);
        const now = { camera: local?.camera === true, screen: local?.screen === true };
        const key = `${now.camera} ${now.screen}`;
        if (key !== last) { last = key; listener(now); }
      });
      return () => sub.remove();
    },
    videos: (videos) => voice.remoteVideos(videos),
    sendData: (data) => voice.sendData(Array.from(data)),
    data: (listener) => {
      const sub = voice.addListener('data', ({ data }) => listener(Uint8Array.from(data)));
      return () => sub.remove();
    },
  };
}

/**
 * A native call, drawn as the Vibez voice screen: a tile per person whose
 * border lights while they speak, their camera filling it, a shared screen on a
 * stage with the people in a strip, and the same control buttons, camera and
 * screen only where the provider takes them. A lost call shows "Reconnecting…"
 * while the engine joins again; one that ends stays on screen with the reason.
 */
function NativeCall({ c, client, snapshot, onClose }: { c: Colors; client: RestClient; snapshot: VoiceSnapshot | null; onClose: () => void }) {
  // A provider that gives only ids (a Slack huddle): each name read once.
  const [names, setNames] = useState<Readonly<Record<string, string>>>({});
  const asked = useRef(new Set<string>());
  const unnamed = (snapshot?.participants ?? []).filter((p) => !p.local && !p.name).map((p) => p.identity).join(' ');
  useEffect(() => {
    for (const identity of unnamed.split(' ').filter((id) => id !== '' && !asked.current.has(id))) {
      asked.current.add(identity);
      void callPersonName(client, identity).then(
        (name) => {
          if (name !== null) setNames((known) => ({ ...known, [identity]: name }));
        },
        () => {},
      );
    }
  }, [client, unnamed]);
  const t = useT();
  const [area, setArea] = useState({ width: 0, height: 0 });
  useEffect(() => {
    const subscription = VoiceNative?.addListener('videoDeclined', () => notify(t('call.videoDeclined')));
    const screen = VoiceNative?.addListener('screenUnavailable', () => notify(t('call.screenUnavailable')));
    return () => { subscription?.remove(); screen?.remove(); };
  }, [t]);
  if (snapshot === null || snapshot.state === 'connecting' || snapshot.state === 'idle') {
    return (
      <View style={styles.center}>
        <ActivityIndicator color={c.accent} size="large" />
        <Text style={[styles.loadingText, { color: c.dimmed }]}>{t('call.connecting')}</Text>
      </View>
    );
  }
  if (snapshot.state === 'disconnected') {
    return (
      <View style={styles.center}>
        <Text style={[styles.loadingText, { color: c.text }]}>{t('call.ended', { reason: endReason(t, snapshot.reason) })}</Text>
        <Pressable onPress={onClose} accessibilityRole="button" style={[styles.finish, { backgroundColor: c.card }]}>
          <Text style={[styles.controlText, { color: c.text }]}>{t('common.close')}</Text>
        </Pressable>
      </View>
    );
  }
  const muted = snapshot.microphone === false || snapshot.deafened === true;
  const cards: VoiceCard[] = snapshot.participants.map((p) => ({
    uid: p.identity, name: p.local ? t('voice.you') : (p.name || names[p.identity] || p.identity), speaking: p.speaking, muted: p.muted,
    deafened: p.deafened === true, local: false, camera: p.camera === true, screen: p.screen === true, mutedHere: false,
  }));
  const sends = callSends(snapshot);
  const sharer = cards.find((card) => card.screen);
  const layout = arrangeTiles(cards.length, area.width, area.height);
  const rows: VoiceCard[][] = [];
  for (let i = 0; i < cards.length; i += layout.columns) rows.push(cards.slice(i, i + layout.columns));
  return (
    <View style={styles.full}>
      <Text style={[styles.status, { color: snapshot.state === 'connected' ? c.online : c.dimmed }]}>
        {snapshot.state === 'reconnecting' ? t('voice.reconnecting') : t('voice.connected')}
      </Text>
      {sharer !== undefined && VoiceVideoView !== null ? (
        <ScreenStage c={c} client={null} sharer={sharer} cards={cards} />
      ) : (
        <View style={styles.grid} onLayout={e => setArea({ width: e.nativeEvent.layout.width, height: e.nativeEvent.layout.height })}>
          {area.width > 0 && rows.map(row => (
            <View key={row[0]!.uid} style={styles.gridRow}>
              {row.map(card => (
                <VoiceTile key={card.uid} c={c} client={null} card={card} width={layout.width} height={layout.height} you={t('voice.you')} />
              ))}
            </View>
          ))}
        </View>
      )}
      <View style={styles.controls}>
        <ControlButton c={c} label={muted ? t('voice.unmute') : t('voice.mute')} icon={muted ? 'microphone-disabled' : 'audio-input-microphone'}
          active={muted} onPress={() => void VoiceNative?.setMicrophone(muted)} />
        <ControlButton c={c} label={snapshot.deafened ? t('voice.undeafen') : t('voice.deafen')}
          icon={snapshot.deafened ? 'audio-volume-muted' : 'audio-headphones'} active={snapshot.deafened === true}
          onPress={() => void VoiceNative?.setDeafened(!snapshot.deafened)} />
        {sends.camera && (
          <ControlButton c={c} label={snapshot.camera ? t('voice.cameraOff') : t('voice.camera')} icon="camera-web" active={snapshot.camera === true}
            onPress={() => void toggleCamera(snapshot.camera === true, t)} />
        )}
        {sends.screen && (
          <ControlButton c={c} label={snapshot.sharing ? t('voice.stopScreen') : t('voice.shareScreen')} icon="video-display" active={snapshot.sharing === true}
            onPress={() => void (snapshot.sharing ? VoiceNative?.stopScreenShare() : VoiceNative?.startScreenShare())} />
        )}
        <ControlButton c={c} label={t('voice.leave')} icon="call-stop" danger onPress={onClose} />
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  status: { fontFamily: FONTS.bodyBold, fontSize: 13, textAlign: 'center', paddingTop: 12 },
  grid: { flex: 1, margin: 12, gap: TILE_GAP, justifyContent: 'center' },
  gridRow: { flexDirection: 'row', justifyContent: 'center', gap: TILE_GAP },
  controls: { flexDirection: 'row', justifyContent: 'center', gap: 12, padding: 20 },
  controlText: { fontFamily: FONTS.bodyStrong, fontSize: 15 },
  full: { flex: 1 },
  veil: {
    position: 'absolute',
    top: 0,
    left: 0,
    right: 0,
    bottom: 0,
    backgroundColor: '#000',
    alignItems: 'center',
    justifyContent: 'center',
  },
  center: { flex: 1, alignItems: 'center', justifyContent: 'center', gap: 14, padding: 24 },
  bar: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    gap: 12,
    paddingHorizontal: 16,
    paddingVertical: 10,
    borderBottomWidth: StyleSheet.hairlineWidth,
  },
  title: { fontFamily: FONTS.title, fontSize: 16, flexShrink: 1, flexGrow: 1 },
  chat: { padding: 4 },
  finish: { borderRadius: 20, paddingHorizontal: 16, paddingVertical: 8 },
  finishText: { fontFamily: FONTS.bodyStrong, fontSize: 14 },
  loadingText: { fontFamily: FONTS.body, fontSize: 14 },
  errorMessage: { fontFamily: FONTS.bodyBold, fontSize: 15, textAlign: 'center' },
  retry: { paddingVertical: 8, paddingHorizontal: 16 },
  retryText: { fontFamily: FONTS.bodyBold, fontSize: 15 },
});
