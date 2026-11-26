# Native calls (research, 2026-11-02)

The goal: calls on Teams, Rocket.Chat (Jitsi), kChat (kMeet) and Slack (huddles) carried
by the apps themselves, with no web page, no WebView and no browser: our own signalling
client per provider, our own WebRTC media, the existing call UI. This page records what the
research and the captures established, the architecture chosen, what is built ("Implementation
status": Jitsi and kMeet, Slack huddles, Teams calls placed from a chat, Mattermost's Calls plugin, WhatsApp 1:1 calls, video, Teams' video), the order, and the captures still owed.
The fallback (a call window on the provider's web page, Teams' one-time Microsoft sign-in) is in
[../features/calls.md](../features/calls.md); the decision is in [../decisions.md](../decisions.md).

## What the apps already ship

- **Desktop**: the `rv-voice` sidecar (`apps/desktop/voice`) links `livekit` 0.9.3, which
  re-exports `libwebrtc` 0.3.50 as `livekit::webrtc`: a full PeerConnection API (offer and
  answer, SDP, data channels, native audio and video sources). The audio chain in
  `audio.rs` (cpal, echo cancellation, RNNoise, voice gate, per-person mix) does not depend
  on LiveKit. The sidecar speaks JSON lines (`crates/rv-voice-protocol`). GStreamer is not
  linked anywhere in the desktop app.
- **Android**: Vibez voice is the local Kotlin module `modules/voice` over
  `io.livekit:livekit-android`, which bundles its own libwebrtc build with the `org.webrtc`
  PeerConnection API (ROADMAP §4.2 "1b"). `crates/rv-voice-mobile` is only RNNoise over
  JNI. Other providers therefore need no new native WebRTC dependency, and the
  `react-native-webrtc` and Jitsi RN SDK paths ROADMAP §4.2 rejects stay rejected.

## Architecture

- **`crates/rv-call`** (pure Rust, no media): one `CallSignaling` trait (join, leave, the
  local offer or answer, remote descriptions, ICE servers, roster, mute state, data-channel
  messages) with one adapter per protocol: `jitsi`, `chime`, `teams`, and `livekit` (the
  existing Vibez path, untouched).
- **Desktop**: `rv-voice` gains a generic connection mode driving `livekit::webrtc` from the
  adapter, and `rv-voice-protocol` a provider-tagged `connect`. The audio chain, devices,
  noise removal, participant and speaking events, the GTK and SwiftUI voice UI and the ring
  prompt are reused as they are.
- **Android**: `rv-call` over JNI as `rv-voice-mobile` is; a Kotlin `GenericCallEngine`
  beside `VoiceEngine` on the bundled `org.webrtc`; `VoiceService` and the call-style
  notification reused.
- **Fallback**: each adapter replaces the provider's call window only once proven; the
  WebView stays as the fallback until then, after which the ROADMAP §4.2 exception shrinks.

## Rocket.Chat (Jitsi) and kChat (kMeet): one adapter

Both are Jitsi Meet: XMPP over the server's `xmpp-websocket`, a MUC room, the focus
(Jicofo) sends a Jingle `session-initiate` once a second participant joins (alone in the
room no media session opens), the client answers, sources are added and removed with
`source-add` / `source-remove`. Media is plain WebRTC to the bridge (JVB): one BUNDLE,
DTLS-SRTP, the server offering (`a=setup:actpass`, the client answers `active`). The bridge
channel (dominant speaker, receiver video constraints) runs over the SCTP data channel when
`bridgeChannel.preferSctp`, else a `colibri-ws` WebSocket.

**chat.barrut.me** (captured live 2026-11-02, call started in `#test`):
- the provider is the Jitsi app (`video-conference.capabilities` `providerName: "jitsi"`,
  `VideoConf_Default_Provider = jitsi`); `video-conference.start` then
  `video-conference.join {callId, state}` answer `{url, providerName, callId, rid}`;
- the Jitsi server is **self-hosted, `meet.barrut.me`** (docker-jitsi-meet defaults): the
  join URL is `https://meet.barrut.me/RocketChat<call id>` with only hash parameters
  (`config.callDisplayName`, `config.startWithAudioMuted`, `config.startWithVideoMuted`,
  `config.prejoinPageEnabled`, `config.prejoinConfig.enabled`, `userInfo.displayName`,
  `config.desktopSharingChromeExtId`): **no JWT, anonymous XMPP**;
- `config.js`: `hosts.domain = meet.jitsi`, `hosts.muc = muc.meet.jitsi`,
  `websocket = wss://meet.barrut.me/xmpp-websocket` (the client adds `?room=<room>`),
  `bosh = https://meet.barrut.me/http-bind`, `bridgeChannel.preferSctp = true`,
  `p2p.enabled = true` (two people go peer to peer; a native client can disable it in its
  own MUC presence), `flags.sourceNameSignaling`, `sendMultipleVideoStreams`,
  `receiveMultipleVideoStreams` true, no E2EE configuration (the web client reports E2EE
  supported, off);
- the bridge's offer (2026-11-02): `a=group:BUNDLE 0 1 2`, audio `opus/48000/2` +
  `telephone-event`, extmaps ssrc-audio-level and transport-wide-cc; video AV1, VP8, H264,
  VP9 each with rtx, extmaps dependency-descriptor, abs-send-time, transport-wide-cc;
  `m=application UDP/DTLS/SCTP webrtc-datachannel` port 5000; the JVB candidate on UDP 10000
  of the server's public address; the peer connection had **no ICE servers** (no STUN, no
  TURN) and `bundlePolicy: max-bundle`.

**kMeet** (`kmeet.infomaniak.com/config.js`, read 2026-11-02; a live kChat call was not
captured, the kChat app being a cross-origin iframe inside kSuite the tools could not
drive):
- `hosts.domain = kmeet.infomaniak.com`, `hosts.muc = muc.kmeet.infomaniak.com`,
  `websocket = wss://kmeet.infomaniak.com/xmpp-websocket`, `bosh = /http-bind`;
- token auth: `tokenAuthUrl = https://welcome.infomaniak.com/api/components/meet/jwt?room={room}`;
  the `jwt` from `POST /conferences` and `/conferences/<id>/answer` (`docs/MATTERMOST.md`)
  is the XMPP credential, passed as `?room=<room>&token=<jwt>` on the websocket as
  lib-jitsi-meet does;
- P2P off (`enableP2P`, `p2p.enabled` false), `useStunTurn` true, `useTurnUdp` false (TURN
  over TCP/TLS only, the servers announced by XEP-0215 external services),
  `bridgeChannel.preferSctp` true, no E2EE configuration.

Reusable code: [gst-meet / lib-gst-meet](https://github.com/jbg/gst-meet) (Rust, MIT or
Apache-2.0) has the XMPP, Jingle, colibri layers and JWT login; take its signalling half
and plug libwebrtc instead of GStreamer and libnice. The Jingle to SDP translation follows
lib-jitsi-meet's `SDP.js`. Risks: Jitsi E2EE (per-frame keys over XMPP, "insertable
streams") is not LiveKit's frame encryption, so an encrypted Jitsi call is refused at
first. Feasibility high; desktop audio about 3 to 4 weeks, video 2 to 3 more, Android 2 to 3.

## Slack huddles (Amazon Chime SDK)

Confirmed in the user's capture (`app.slack.com_huddle.har`, 2026-11-02, a huddle started
in `#caca`):
- `POST /api/rooms.join` (form: `token`, `channel_id`, `regions`, `multidevice`, the usual
  `_x_*`) answers `call.call_id` and `call.free_willy` = **Chime meeting and attendee
  credentials**: `meeting.MeetingId`, `ExternalMeetingId`, `MeetingArn` (AWS us-east-1
  control plane), `MediaRegion` (`eu-west-3` here), `MediaPlacement.{AudioHostUrl
  (host:3478), AudioFallbackUrl (wss), SignalingUrl (wss://signal.<cell>.m.chime.aws/control/<meeting id>),
  TurnControlUrl (https://<n>.cell.us-east-1.meetings.chime.aws/v2/turn_sessions), ScreenDataUrl,
  ScreenSharingUrl, ScreenViewingUrl, EventIngestionUrl}`, `attendee.{AttendeeId,
  ExternalUserId, JoinToken, Capabilities.{Audio,Video,Content}}`; `huddles.get {huddle_id}`
  reads the huddle (participants, channels, `huddle_link`);
- the signalling WebSocket is `SignalingUrl` with query `X-Chime-Control-Protocol-Version`,
  `X-Amzn-Chime-Send-Close-On-Error`, `X-Amzn-Version`, `X-Amzn-User-Agent`, the JoinToken
  as `Sec-WebSocket-Protocol` (`_aws_wt_session`); binary protobuf frames (first ones 24 B
  each way, then a 152 B join, 288 B join ack, a 2324 B frame carrying the SDP offer);
- the first capture missed the TURN step and the SDP; the second one
  (`app.slack.com_huddle2.har` plus a `webrtc_internals_dump.gz` "Create dump", 2026-11-02)
  has the whole signalling WebSocket, decoded with the SDK's own
  [SignalingProtocol.proto](https://github.com/aws/amazon-chime-sdk-js/blob/main/protocol/SignalingProtocol.proto).

**The whole Chime flow, end to end** (second capture):
1. `rooms.join {channel_id, regions, multidevice}` → `call.free_willy` as above (a huddle
   in a channel: `ExternalMeetingId` = `<team>-<call id>-<user>`, `huddle.external_unique_id`
   = the Chime `MeetingId`);
2. WebSocket to `SignalingUrl` with the JoinToken as subprotocol. Each binary frame is one
   type byte then a protobuf `SdkSignalFrame {timestamp_ms, type, <one body>}`; the client
   sends type `0x05`, the server's frames start with `0x02`
   ([DefaultSignalingClient.ts](https://github.com/aws/amazon-chime-sdk-js/blob/main/src/signalingclient/DefaultSignalingClient.ts));
3. the sequence: `PING_PONG`, then `JOIN {protocol_version 2, flags, client_details
   (client_source "amazon-chime-sdk-js", chime_sdk_version 3.32.0, platform, os),
   audio_session_id, wants_compressed_sdp, server_side_network_adaption}` (113 B);
   `JOIN_ACK {turn_credentials {username, password, ttl 300, uris: turn:ice.<cell>.m.chime.aws:3478?transport=udp,
   turns:...:443?transport=tcp}, video_subscription_limit 25, wants_compressed_sdp}`;
   `INDEX {num_participants, at_capacity, supported_receive_codec_intersection}`;
   `SUBSCRIBE {duplex RX, send_streams [{stream_id, group_id, track_label
   "AmazonChimeExpressAudio", media_type AUDIO, attendee_id, max_bitrate_kbps}],
   audio_host (the AudioHostUrl, :3478), audio_muted, compressed_sdp_offer}`;
   `SUBSCRIBE_ACK {duplex, allocations [{stream_id, group_id}], compressed_sdp_answer}`;
   `AUDIO_STREAM_ID_INFO {streams [{audio_stream_id, attendee_id, external_user_id, muted}]}`
   (who is in the huddle), then every ~100 ms `AUDIO_METADATA` (levels), every second
   `CLIENT_METRIC` (sent) and `BITRATES`, `DATA_MESSAGE {topic "awareness-sync-channel"}`,
   a `PING_PONG` every 10 s. No `LEAVE` in the capture (it ends before hanging up);
4. **TURN comes in the JOIN_ACK**: the capture has no request to `TurnControlUrl`, and the
   peer connection is created with
   `iceTransportPolicy: relay`, `bundlePolicy: max-bundle` and that single TURN host
   (webrtc-internals): all media goes through Chime's TURN;
5. **the SDP is compressed**: `deflateRaw` (pako); the server seeds each answer after the
   first on a socket with the previous answer as dictionary, the client's offers inflate
   with none (third capture, below)
   ([ZLIBTextCompressor.ts](https://github.com/aws/amazon-chime-sdk-js/blob/main/src/sdp/ZLIBTextCompressor.ts));
6. the SDP itself is plain WebRTC: client offer `setup:actpass`, server answer `passive`,
   `BUNDLE 0 1` (audio `sendrecv`, video `inactive` until a camera or share), `rtcp-mux`,
   UDP/TLS/RTP/SAVPF; offer codecs Opus, red, G722, PCMU/PCMA, CN, telephone-event, and for
   video VP8, VP9, H264, AV1 with rtx/red/ulpfec; the answer keeps Opus+red and
   VP8/VP9/H264/AV1+rtx; header extensions transport-wide-cc, abs-send-time, sdes:mid,
   rid, ssrc-audio-level (offer only), dependency-descriptor, video-layers-allocation.
   A second offer/answer pair follows once connected (renegotiation).

So the Slack side gives everything a native Chime client needs: credentials, signalling
sequence, TURN, SDP; built, see "Slack huddles" under "Implementation status". Video and
the screen share were decoded from a third capture, see "Slack huddles over Chime, video".
Still unseen: the main socket's `LEAVE` (the SDK's own is what is sent).

Media is standard WebRTC with the client always offering. AWS publishes the signalling for
custom media stacks: [amazon-chime-sdk-cpp signaling](https://github.com/aws/amazon-chime-sdk-cpp/blob/main/chime-sdk-signaling-cpp/README.md)
(its demo uses libwebrtc) and the `.proto` in
[amazon-chime-sdk-js](https://github.com/aws/amazon-chime-sdk-js/blob/main/src/signalingclient/SignalingClient.ts)
(Apache-2.0), which generate into Rust. The Chime Android SDK's media layer is a closed
binary, not usable. Risks: Slack's terms forbid unofficial clients (the xoxc session is
already that grey zone). Feasibility medium-high; audio about 3 to 5 weeks.

## Microsoft Teams

From the user's HARs (2026-11-01/02, see [teams.md](teams.md) Live and Calls):
- ringing is Trouter `evt` 107 on the `SkypeSpacesWeb` registration; the ringing side calls
  `cc.skype.com` through `api.flightproxy.teams.microsoft.com/api/v2/ep/<host>/...`:
  `forked/.../attach` (registers its Trouter `callAgent` callback links), `incoming/.../progress`,
  `accept {callAcceptance, acceptedCallModalities, mediaRenegotiation links, SDP}`,
  `DELETE .../reject {callEnd}`; the conversation service on `conv.skype.com`
  (`updateEndpointState`, a group call's POST `conv/<id>` with `groupChat.threadId` and
  `meetingInfo`, `updateEndpointMetadata`, `leave`); the broker (`broker.skype.com`) is a
  30 s long poll;
- **auth: an `https://ic3.teams.office.com` bearer** (`scp` `Teams.AccessAsUser.All`) on every
  calling request (cc, conv, broker, `epconv`), read from "with sensitive data" HARs
  (2026-11-02, claims only). **Our own client mints it**: the device-code refresh token
  (client `1fec8e78`) redeemed for `https://ic3.teams.office.com/.default` answers 200 with
  the same `aud` and `scp` (`examples/teams-ic3.rs`, live, 2026-11-02). No FOCI hop is
  needed, unlike the web window. Whether the calling stack accepts our client's token is
  not yet proven: the only harmless GET available, an expired broker `subscribe` link,
  answers 404 `subCode 1001` ("message channel does not exist") with or without a token,
  so a first real call settles it;
- **outgoing 1:1 call** (`teams.cloud.microsoft_call_out.har`): one
  `POST https://api-emea.flightproxy.teams.microsoft.com/api/v2/epconv` (201) carrying
  `conversationRequest` (roster `Delta` with a `rosterUpdate` link, properties
  `allowConversationWithoutHost`, `enableGroupCallEventMessages`, and the conversation
  callbacks `conversationEnd`, `conversationUpdate`, `localParticipantUpdate`,
  `addParticipantSuccess/Failure`, `addModalitySuccess/Failure`, `confirmUnmute`,
  `receiveMessage`), `participants {from: {id 8:orgid:me, displayName, endpointId,
  participantId, languageId}, to: [{id 8:orgid:callee, participantId}]}`,
  `endpointCapabilities`, `clientEndpointCapabilities`, `endpointState`, and
  `callInvitation {callModalities: ["Audio"], links {progress, mediaAnswer, acceptance,
  redirection, end}, clientContentForMediaController {controlVideoStreaming, csrcInfo},
  mediaContent {blob: <SDP offer, setup:actpass>}}`. Every callback link is a path of
  the client's own Trouter `SkypeSpacesWeb` registration:
  `https://<trouter host>:3443/v4/f/<registration path>/callAgent/<endpoint>/<random>/...`.
  The answer is a `conversationController` URL plus ~30 conversation links (`leave`,
  `mute`/`unmute`, `addParticipant`, `addModality`, `updateEndpointState`,
  `updateEndpointMetadata`, `subscribe` and `brokerPublish` on the broker,
  `sendMessage`, ...), `state {isMultiParty, isMeetingActivated}` and `callLimits`.
  Then the broker long poll, `updateEndpointMetadata`, and `POST .../leave` (204) to hang
  up. The callee's SDP answer arrives on the `mediaAnswer` callback, i.e. over Trouter:
  these HARs hold no Trouter frames (its socket opened before recording), so its exact
  shape is still owed;
- **incoming call answered** (`..._call_in.har`): `forked/.../attach` (`requireMediaContent:
  false`, an `end` callback and an `additionalActions` conversation request with the same
  callbacks), `incoming/.../progress`, broker subscribe, a conv POST, then
  `incoming/.../accept {callAcceptance {acceptedBy, acceptedCallModalities: ["Audio"],
  links {mediaRenegotiation, transfer, replacement, balanceUpdate, retargetCompletion,
  controlVideoStreaming, updateMediaDescriptions}, mediaContent {blob: <SDP answer,
  setup:active, application sharing rejected with port 0>}}}`, `updateEndpointMetadata`,
  `leave`;
- **group call vs meeting**: both "group call" captures (`..._group_call.har`,
  `..._group_call2.har`) are **calls of a group chat**, not meetings: `epconv` with
  `groupChat {threadId: 19:...@thread.v2}`, `meetingInfo: null`, two `to` participants,
  `callModalities ["Audio", "ScreenViewer"]`, and an offer with one audio, ten
  `main-video`, one `applicationsharing-video` and the `x-data` m-lines. A meeting
  (`meetingInfo`, meetup-join) has not been captured;
- **screen sharing** (`..._screenshare.har`, 2026-11-02, the same `ic3` bearer on every
  request): starting a share is `POST {cc}/cc/v1/active/<call>/.../renegotiate
  {mediaNegotiation {callModalities, sender, links {mediaAnswer, rejection}, mediaContent
  {blob: SDP offer, negotiationTag, mediaLegId}}}` whose offer turns the
  `applicationsharing-video` m-line `sendonly` (audio `sendrecv`, `main-video` `inactive`,
  `x-data` `sendrecv`), plus `conv/<id>/updateParticipantProperties` with a
  `screenSharerAttributes` property; the answer comes back on the `mediaAnswer` callback
  and is confirmed by `POST {cc}/cc/v1/mediaAnswers/.../acknowledge`. Stopping is another
  `renegotiate` with that m-line rejected (one payload type, no direction). When the other
  side shares, the far end renegotiates and the client answers on
  `{cc}/cc/v1/negotiations/.../answer {mediaAnswer {mediaContent, clientContentForMediaController
  {controlVideoStreaming, csrcInfo}, links {mediaAcknowledgement}}}` with that m-line
  `recvonly`. The screen is captured at up to 1920x1080, 15 fps (`getDisplayMedia`,
  webrtc-internals);
- **still missing**: the Trouter frames of a call (the `mediaAnswer` on an outgoing call,
  the ring in a fresh session), where the relay (TURN/MRAS) credentials come from (no
  such request in these HARs), a meeting join, and a `chrome://webrtc-internals` dump of a
  Teams call (the 2026-11-02 dump holds only Slack's peer connection);
- the SDP (`application/sdp-ngc-1.0`): DTLS-SRTP, shared transport, Opus/G722/PCM, H264/AV1,
  transport-wide-cc, plus Microsoft extensions: an `m=x-data` SCTP channel,
  `a=x-ssrc-range` (100 video SSRCs reserved in a group call),
  `a=x-signaling-fb:* x-message app send:src recv:src,vc`, `a=label:main-audio` /
  `main-video` / `applicationsharing-video`, the profile written `RTP/SAVP`; libwebrtc
  carries it with SDP rewriting both ways;
- the window cannot be signed in for the user: the FOCI exchange to Teams' web client is
  refused (`AADSTS70000`, [teams.md](teams.md) Calls), so native is the only sign-in-free path.

Reference: [Microslop](https://github.com/brandonp2412/microslop) (MIT, Rust core) reports
1:1 audio against Microsoft's Test Call, video partial. Risks: Teams E2EE 1:1 calls and
Premium meeting E2EE are likely unjoinable; Microsoft changes the protocol silently and its
terms forbid unofficial clients. Feasibility medium for 1:1 audio (4 to 6 weeks), low for
meetings with video (2 to 3 months more).

## Implementation status

**Jitsi, desktop audio: built and checked live (2026-11-02).**
- `crates/rv-call` (own workspace, like the provider crates; CI in `providers.yml`): the `CallSignaling`
  trait (`next_event`, `answer`, `local_candidate`, `set_muted`, `leave`, `local_id`) and the Jitsi
  adapter. `jitsi/mod.rs` reads a meeting's join parameters from the host's `config.js` (both the
  object style and docker-jitsi-meet's `config.hosts.muc = 'muc.' + subdomain + ...` assignments;
  `Config::is_jitsi`), with no network code; `jitsi/client.rs`, behind the default `client` feature,
  is the XMPP connection: WebSocket with the `xmpp` subprotocol, SASL ANONYMOUS (kMeet's JWT on the
  URL as `?room=&token=`), resource bind, XEP-0215 TURN servers, the focus asked for the conference
  (retried while `ready='false'`), MUC presence (nick, `SourceInfo`, `audiomuted`), disco#info answered
  with audio-only features (no `rtp:video`, no `dtls-sctp`: Jicofo then offers one `audio` content and
  no data channel), XMPP pings every 25 s. `jitsi/sdp.rs` turns the focus's `session-initiate` into a
  Unified Plan offer: one `sendrecv` section (`mid` = the content name) for this client's microphone,
  then one `sendonly` section per remote audio source (`mid` `r<n>`, `a=msid:<endpoint> <source name>`,
  the bridge's own `jvb` sources skipped); `source-add` appends sections, `source-remove` makes them
  `inactive` with port 0 (never removed), each a local renegotiation. libwebrtc's answer becomes the
  `session-accept` (the first section's codecs, header extensions, our source `<endpoint>-a0`, ICE and
  DTLS `setup='active'`); later candidates go as `transport-info`. A peer-to-peer `session-initiate`
  from another occupant is declined with `session-terminate`, which keeps the call on the bridge.
- `rv-voice` (`call.rs`): `connect_jitsi` (rv-voice-protocol) runs one call: rv-call's session, one
  libwebrtc `PeerConnection` of its own (`PeerConnectionFactory::default`, the ICE servers XEP-0215
  gave), the microphone track from the same `NativeAudioSource` as LiveKit's, each remote track heard
  through `Audio::listen_rtc` under its stream id (the endpoint), participants with the room's display
  names (`Participant.name`), mute (track disabled and the MUC presence updated), deafen, devices,
  volumes.
- rv-core `VoiceController::connect_jitsi` (one start path with LiveKit's `connect`) and
  `voice::discover_jitsi` (the meeting host's `config.js`, 5 s); GTK `native_call.rs`: a Rocket.Chat
  call whose meeting is on a Jitsi Meet server opens a small native call window (who is in the call,
  who speaks, mute, deafen, hang up, length); no sidecar or a failed join opens the meeting's page.
- **Checked live** on meet.barrut.me: login, conference, room join and the audio-only offer with a
  browser in the room (signalling probe `examples/jitsi-join.rs`); then two sidecars in one test room,
  each sending a tone (`RV_VOICE_FAKE_AUDIO=sine`), each heard the other through the bridge (level 0.8,
  speaking) and left cleanly. With a web participant (2026-11-02, Jitsi Meet in Chrome, muted, joined to the
  same test room): the call stayed on the bridge (Jicofo's peer-to-peer offer declined), the browser
  received the native client's Opus and heard it (inbound level 0.37 once the page could play audio:
  an unclicked tab keeps its remote `<audio>` paused by autoplay rules and reports level 0), and the
  browser's hang-up removed it from the native roster at once (a closed tab only leaves when the
  server times it out). Browser to native was not run with a live microphone; two native clients
  hear each other through the bridge. kMeet (2026-11-02): the GTK call path takes kChat calls too, the
  `?jwt=` of the conference answer's URL becoming the XMPP token (`voice::meeting_token`, unit-tested).
  Live on kMeet (2026-11-04), in `rocketvibe-native-test-*` rooms: kMeet admits an anonymous join (no
  token): login, conference and room, one TURN server `turns:coturn.infomaniak.com:443?transport=tcp`
  (XEP-0215), the bridge's offer (1 audio section, 1 candidate) to each of two probes; two `rv-voice`
  sidecars (`RV_VOICE_FAKE_AUDIO=sine`) each heard the other at level 0.8. An invalid `?jwt=` makes
  SASL ANONYMOUS answer failure (`login refused`): the token is checked at login. A call started
  from kChat (a valid JWT) was not run natively: its token stayed in the browser (Chrome refused
  handing it to a local probe), so the user's "native kMeet call does not work" is not reproduced;
  GTK now logs why a native join fell back to the page (`native_call.rs`), the sidecar its rv-call
  error (`call: join failed: ...`). Not yet run: the GTK window itself against a real Rocket.Chat call (the
  sidecar path under it was), kMeet live, SwiftUI on a Mac, Android on a device, video.
- **SwiftUI (2026-11-02)**: rv-ffi `jitsi_call.rs` exports `start_jitsi_call(meeting_url, display_name,
  title)`, which discovers the meeting (`voice::discover_jitsi`, kMeet's `?jwt=` included) and joins it
  on a `VoiceController` of its own (one sidecar per call, as GTK's window), and `JitsiCallSession`
  (`state`: connected, ended, microphone, deafened, people with name, muted, speaking;
  `set_microphone`, `set_deafened`, `leave`). `CallWindow.show` tries it first when given the account's
  `displayName` (room call button, call card's Join, a Rocket.Chat or kChat ring answered on the open
  account) and opens `NativeCallWindow` (SwiftUI in an `NSWindow`, polled every 300 ms), the
  WKWebView page otherwise; a background account's ring and a profile's call button keep the page.
  Checked by the Linux build of RocketVibeKit's bindings and the macOS CI only.
- **Android (2026-11-02)**: `crates/rv-voice-mobile` (`call.rs`) wraps rv-call over JNI for
  `com.rocketvibe.voice.JitsiCall`: `start` (meeting URL, its host's `config.js` fetched by Kotlin,
  display name, kMeet's token) joins on a Tokio runtime of its own and returns a handle from a
  registry (a late call after `free` finds nothing; no raw pointer crosses JNI); a reader thread polls
  `next` for the session's events as JSON (`local`, `ice`, `offer`, `candidate`, `roster`, `ended`)
  and is the only one to `leave` and `free`; `answer`, `candidate` and `muted` go through the session's
  command channel. rv-call's `webpki-roots` feature adds Mozilla's roots, Android having none
  rustls-native-certs can read. `JitsiSession` (`JitsiCall.kt`) answers the bridge's offer with the
  `livekit.org.webrtc` LiveKit's SDK ships (its own `PeerConnectionFactory` and `JavaAudioDeviceModule`,
  hardware echo canceller and noise suppressor), hears each remote track by its stream id (the
  endpoint) and reads who speaks from the receivers' `audioLevel` three times a second. `VoiceEngine`
  carries it like a LiveKit room (`connectJitsi`: the same snapshot, `VoiceService`, mute and deafen),
  and the call screen (`app/call/[callId].tsx`) joins natively first for a Rocket.Chat or kChat call
  (`lib/call.ts` `nativeJitsi`), showing who is in the call and its controls, then falls back to the
  WebView for good if the join fails. Checked: host and Android cross-compile of the crate, its unit
  tests, the module's Kotlin against LiveKit's WebRTC (`:voice:compileDebugKotlin`); not run on a
  device (an emulator call needs a signed-in Rocket.Chat account).

### Slack huddles over Chime, audio (2026-11-06)

- **rv-call `chime`**: `proto.rs` writes and reads the few protobuf messages by hand (field numbers
  of the SDK's `SignalingProtocol.proto`), `mod.rs` builds the frames (`join`, `subscribe`,
  `audio_control`, `leave`, `ping_pong`) and reads who is in the call (`Roster`), `client.rs` is the
  socket: the join token as `Sec-WebSocket-Protocol: _aws_wt_session, <token>`, JOIN, JOIN_ACK's TURN
  servers, then the engine offers. The trait gained that direction: `Event::OfferWanted { relay_only }`,
  `CallSignaling::offer`, `Event::Answer`, as Chime's client always offers and takes no trickle. The
  SDP is raw deflate (`miniz_oxide`); the server's answers after the first need the previous one as
  dictionary (`inflate_answer`, see the video section).
  The server mixes everyone into one audio stream (`msid:audio audio`), so who speaks comes from
  AUDIO_METADATA, `Event::Levels` (minus decibels over the SDK's -42 to -14 dB); the volume of one
  person cannot be set.
- **Ending**: AUDIO_STATUS 200 is fine; 301 (joined elsewhere), 403, 409, 410 (meeting ended), 411
  (removed) are `Event::Over`, which is not rejoined; anything else is a loss, rejoined like Jitsi's
  with the same token. A meeting ends when its last attendee leaves: a rejoin then answers `refused:
  meeting unavailable`.
- **Sidecar** (`call.rs`): `Peer::Gathered`, the offer waiting for ICE. libwebrtc's
  `current_local_description` stays empty until the answer is set, so the offer's text gets the
  gathered candidates itself (`rv_call::sdp_with_candidates`). It goes when gathering completes, 500 ms
  after the first candidate (live, gathering had not said it was done after 5 s), or after 5 s.
- **Slack**: `rooms.join {channel_id, multidevice: false}` starts or joins the room's huddle
  (`regions` is optional: Slack then picks). `multidevice: true` gives the attendee a device suffix
  (`<team>-<huddle>-<user>-<device>`, `huddle_person` takes the third part). A free workspace answers
  `freemium_max_participants` to a second attendee of the same user. Attendees are named by the app
  (`Session::huddle_person_name`, `callPersonName`), read once each from Slack's directory.
- **Apps**: GTK `open_call` tries it first (`Session::join_huddle`, `native_call::open` with a
  `NativeCall` and `Names`), the huddle's page as the fallback (`open_huddle_page`); rv-ffi
  `Chat::start_huddle_call`, SwiftUI `CallWindow.show(chat:)`; Android `JitsiSession.startChime` over
  `rv-voice-mobile`'s `startChime`/`offer`, the call screen's `joinedHuddle` (microphone permission
  only), the browser as the fallback.
- **Checked live (2026-11-06)**, `rv-voice` with a tone against real huddles in a test channel,
  credentials from `rooms.join`, the user in the same huddle from Slack's web app: join, TURN (UDP
  relay), the answer, `connected` in under 2 s, held 4 min, LEAVE answered. The user heard the tone;
  their arrival, departure, voice levels and mute reached the roster. They heard a few hiccups: the
  stats (`RV_CALL_DEBUG=1`, every 5 s) showed 0 packets lost each way, 13 ms round trip, while the
  tone's own timer stalled 6 times in about 2 min, up to 119 ms, on a machine at load 12.8. So the
  evidence points at the loaded machine and the fake tone, not the path; no hiccup was matched to a
  stall one by one. Inbound runs about 230 packets a second against 50 sent, unexplained (padding
  probes, as the join declares bandwidth probing, is a guess). **Not run**: this client's playback (the
  tone mode plays nothing, so a remote voice was seen as packets and levels, never heard here); a real
  microphone in a huddle; the GTK window on a signed-in Slack account (none on the test machine);
  SwiftUI (macOS CI only); Android on a device (Kotlin and the Android cross-compile only).

### Slack huddles over Chime, video (2026-11-24)

Decoded from `app.slack.com_huddle2.har` (2026-11-23: two attendees of one Slack user, each turning a
camera on, then sharing a screen), both signalling sockets read frame by frame:

- **INDEX** (type 5, field 8) lists every remote video source each time; one with no source means
  none. Each: `stream_id`, `group_id`, `attendee_id`, `media_type` 2, `external_user_id`, width and
  height, field 15 the base stream, field 17 a codec number (8 for a camera, 11 for a screen: VP9 and
  AV1 by inference, the client offering those first). This client's own camera never appears in it.
- **A shared screen is a second attendee**, `<attendee>#content`, on a second socket to the same
  `SignalingUrl`. Its join token is the main one with `#content` appended, made by the client (no HTTP
  call). It JOINs, SUBSCRIBEs duplex 3 sending video only (stream 2, 1500 kbps), receives nothing and
  LEAVEs when the share stops. The main socket sees it in INDEX under the person's external id; with
  the JOIN asking for every temporal layer (field 12, `wants_all_temporal_layers_in_index`) it comes
  as three streams (base, layers 0 and 1, INDEX field 13). `ScreenSharingUrl` / `ScreenViewingUrl`
  were not used. Frame by frame: "Slack huddles over Chime, screen share".
- **Receiving**: SUBSCRIBE `receive_stream_ids` has one entry per video section of the offer, in
  m-line order: the stream to receive, or 0 for a sending or spare section. Each received one gets a
  `video_subscription_configuration` (mid, attendee, priority, target 600 kbps for a camera and
  10000 for a screen, adaptation 2 or 3). SUBSCRIBE_ACK maps the stream to an SSRC labelled `v_<mid>`;
  the answer's section is `sendonly`, `msid:v_<mid> track_<mid>`. A source gone: REMOTE_VIDEO_UPDATE
  (type 24, field 25) with its mid removed, then a SUBSCRIBE with 0 there.
- **Sending the camera**: duplex 3 and a second send stream (stream 2, group 2,
  `AmazonChimeExpressVideo`, media_type 2, 15 fps, 1400 kbps); off, duplex 1 and the audio stream
  alone. One encoding, no simulcast; dependency-descriptor and video-layers-allocation negotiated.
- **Answers are deflated with the previous answer on the same socket as preset dictionary**, every
  one after the first; the offers carry none.
- **Slack itself moves nothing**: `rooms.join` gives `Capabilities` Video and Content `SendReceive`;
  `sh_room_join`'s `participants_camera_on` and `participants_screenshare_on` stayed empty throughout.
  A share announces itself only in Chime data messages (`data-channel`, `browser:content`).

Built (desktop):

- **rv-call** `chime/video.rs`: `index_sources` (one source per attendee, its base stream; this
  attendee and its own `#content` left out), `Subscriptions` (the sources wanted, at most JOIN_ACK's
  `video_subscription_limit` and 16, each kept on its receive-only section across offers; the receive
  ids and configurations built from the offer's video sections; `remote` names each video by the
  answer's stream id `v_<mid>`, as libwebrtc gives a receiver this side created a random track id).
  `mod.rs`: `subscribe` (camera, receive ids, configurations), `remote_video_removed`,
  `inflate_answer` (the dictionary as a stored deflate block before the answer, `miniz_oxide` having
  no preset dictionary). `client.rs`: an INDEX change frees sections, sends `Event::Videos` and
  `Event::OfferWanted`, one SUBSCRIBE in flight at a time; `can_send` the camera, `set_video` offers
  again.
- **The trait** gained `CallSignaling::receive_videos`; the sidecar (`call.rs` `Call::add_receivers`)
  adds receive-only video transceivers up to it before each offer and never removes one. Chime's
  `offers_to_receive_video` turns true once a video was wanted and stays so: libwebrtc's legacy offer
  option at false makes every video section inactive (test
  `offers_carry_the_receiving_sections_asked_and_keep_them`).
- **Windows**: nothing huddle-specific; GTK shows the camera button from `Event::CallVideo`, the
  others' cameras in tiles and a screen on the stage as for Jitsi.
- **Android** (2026-11-26): `rv-voice-mobile`'s `offerWanted` event carries `receiveVideos` and
  `receiveVideo` (the session's `receive_videos` and `offers_to_receive_video`, read as it goes
  out, `session_event_json`); `JitsiSession` (`JitsiCall.kt`) keeps the most asked for in the session,
  adds receive-only video transceivers up to it before each offer (`addReceivers`, none removed) and
  sets `OfferToReceiveVideo` once video was wanted. The answer's `v_<mid>` streams name each track
  through the call's video list (`shown()` matches it by stream); the camera goes on a send-only
  section of its own, as for Mattermost. The call screen needed nothing: its camera button follows
  `canCamera`, true for Chime since the shared code's `can_send(Camera)`.
- **Not built**: this side's screen share on Android (the `#content` attendee, built on the desktop:
  `rv-voice-mobile` runs no second session nor peer connection); PAUSE/RESUME of a group. **Not run** against a
  live huddle: the capture, unit tests and the sidecar's offer test only; Android by compiling (the
  Android cross-compile, `:voice:compileDebugKotlin`) and rv-voice-mobile's tests only.

### Slack huddles over Chime, screen share (2026-11-26)

The `#content` socket of `app.slack.com_huddle2.har`, read frame by frame (a 5 s share):

- **Socket**: the same `SignalingUrl` and query as the main one, subprotocol `_aws_wt_session,
  <join token>#content`. A PING, then a JOIN like the main one but for `server_side_network_adaption`
  1 (field 10) with no supported list (11), `wants_all_temporal_layers_in_index` false (12) and
  `disable_periodic_keyframe_request_on_content_sender` true (13); the main JOIN has 12 and 13 at 1
  (this client's sends neither). JOIN_ACK: TURN credentials of its own, `video_subscription_limit` 25.
  Its INDEX lists no source.
- **SUBSCRIBE**: first duplex 1 with an audio send stream (`AmazonChimeExpressAudio` under
  `<attendee>#content`) and the video section inactive, then duplex 3 adding stream 2, 15 fps,
  1500 kbps, `AmazonChimeExpressVideo`; `receive_stream_ids` one 0, no subscription configuration,
  `audio_host`, not muted. The SDK sends it again, unchanged, on each INDEX (seven in 5 s).
  SUBSCRIBE_ACK allocates the video stream 131 (group 4); BITRATES then names 131 to 133, the three
  temporal layers.
- **SDP**: `BUNDLE 0 1`, audio `sendrecv` with a track (the SDK's silent microphone), one video
  section `sendrecv` with one track; AV1 offered first, then H264, VP8, VP9 with rtx, red, ulpfec; no
  `rid`, no simulcast; the camera's header extensions. The answer: audio Opus and red, `msid:audio
  audio`; video `sendrecv`, H264 first, then VP8, VP9, AV1 with rtx. Answers after the first are
  deflated with the previous one as dictionary, as on the main socket.
- **Otherwise**: it receives AUDIO_METADATA, AUDIO_STREAM_ID_INFO and Slack's `awareness-sync-channel`
  broadcasts, and sends CLIENT_METRIC every second, one MEETING_SESSION_TIMING (type 43) and PINGs.
  LEAVE (type 9, empty) is answered by LEAVE_ACK in 60 ms, then the socket closes.
- **The main socket**: about 4.6 s before the content socket opens (the screen picker), the main
  attendee sends two DATA_MESSAGEs on topic `data-channel`, Slack's own protobuf: `{4: attendee,
  5: empty}` then `{4: attendee, 15: {1: "browser:content", 2: attendee}}`; the other attendee's
  share began the same way. Nothing announces the stop. INDEX then lists `<attendee>#content`, and
  AUDIO_STREAM_ID_INFO adds it as an audio stream under the person's external id until it leaves.
  Both sharers' `awareness-sync-channel` carried `contentView` some seconds into the share (meaning
  unknown; not sent here).

Built (desktop):

- **The trait**: `CallSignaling::screen_apart` and `screen_session` (false and `None` by default):
  a provider taking this side's screen as a session of its own. Other providers are untouched.
- **rv-call** `chime`: `Config::content` (`#content` after the attendee id and the token);
  `Session::screen_session` answers at once with a session whose task connects (`client.rs`
  `connect`) and JOINs as content (`join(.., content)`); its offers go in SUBSCRIBEs with the silent
  microphone and the screen at 1500 kbps; it ignores INDEX, stream infos and levels; a failed
  connection ends it with `Event::Ended`. `can_send(Screen)` is true on the main session, whose
  `set_video` with the screen newly on sends the two `data-channel` messages (`share_announced`). The
  roster drops every `#content` attendee: one counted as a person showed this side's share as another
  participant, and its silence overwrote a sharer's level.
- **Sidecar** (`call.rs`): `Videos.apart` keeps the screen off the main peer connection; once its
  capture is ready and the main peer connection exists, `sync_video` opens a `Share`: the screen
  session and a peer connection of its own from the same factory, a silent microphone (a
  `NativeAudioSource` given no frames) and the screen on a `sendrecv` section as Chrome offers it
  (whatever comes back on it is unused), offered after gathering as the main one is. Its peer connection connected, the main session is told the screen is sent.
  Stopping the capture closes it (peer connection closed, `leave` in the background). A share failing
  reports `screen_unavailable` before it connected and `screen_ended` after, stops the capture, and the
  call goes on. A main session lost or over leaves its share; a rejoin opens a new one while the
  capture runs. The windows needed nothing (`Event::CallVideo` shows the button); SwiftUI shares
  through the same sidecar, whose screen capture works on macOS.
- **Not run** against a live huddle: whether Chime takes a microphone that sends no frame; whether the
  others' Slack clients show the share as they do Chrome's. Checked: the frames against the capture and the sidecar's offer, in unit tests.

### Teams calls placed from a chat, audio (2026-11-06)

- **Who does what**: unlike Jitsi and Chime, the app speaks the signalling, because it needs the
  account's token refresh and Trouter socket; the sidecar only offers and carries the media
  (`Command::ConnectExternal`, `Event::LocalOffer`, `RemoteAnswer`, `RemoteRoster`, `RemoteEnded`; rv-voice
  `call::External`, not rejoinable). Android the same: `JitsiSession.startExternal`,
  `connectExternal` and the `localOffer` event, the signalling in `provider-teams` (`calling.ts`, a port).
- **The flow** (rv-teams `calling.rs`, rv-core `teams_call.rs`): the offer in Teams' dialect, then
  `POST https://api.flightproxy.teams.microsoft.com/api/v2/epconv` with the callbacks under the socket's
  route (`<surl>callAgent/<endpoint>/<random>/...`), bearer audience `https://ic3.teams.office.com`,
  which **our own client's token passes**. The answer arrives over Trouter as `callAcceptance`
  `.mediaContent.blob` (`mediaAnswer` is read too); `links.acknowledgement` wants
  `{callAcceptanceAcknowledgement: {links}}` or the call ends (430). `rosterUpdate` is a delta keyed by MRI
  (`details.displayName`, `endpoints`: empty when gone). `links.leave` hangs up (204).
- **The dialect, found against Teams itself**: port 9 with `c=0.0.0.0` answers
  `DisabledMediaNotSupported`, so the offer names a real address from a candidate, plus `a=rtcp:<port>`;
  profile `RTP/SAVP`, extension URIs with backslashes, `a=label:main-audio`, `x-ssrc-range`, UDP candidates
  only, without `generation`/`ufrag`/`network-id`. An audio-only offer is accepted, and answered without
  the `x-data` a browser's carries a section for. The answer keeps its audio section: `UDP/TLS/RTP/SAVPF`,
  forward slashes, UDP candidates without `MTURNID`, no `tcp-pass`, `a=x-*`, `a=label`, `a=ptime`.
  Teams' media server offers public UDP candidates, so no TURN was needed; the relay token
  (`api/v2/ep/relay/token`, from Microslop) is not used, which a network blocking UDP would need.
- **Keeping it up** (found live, 2026-11-07): the `epconv` answer's `links.subscribe` is the
  conversation's broker, a long poll (`GET`, `x-usebatching: 1`) answering `{message, nextSubscribeUrl,
  expirationTimeInSec: 30}`: a client that stops polling loses its media about 35 s in (ICE disconnected,
  then failed, no `callEnd`), which the echo bot does not show. rv-core `hold_subscription` keeps polling at
  each `nextSubscribeUrl` for the whole call. As Teams' web client does, the app also `PUT`s
  `updateEndpointMetadata` and, once accepted, registers on `links.callLeg`
  (`{callAcceptanceAcknowledgement, callParticipantUpdate}`, both the same links). With these a call with a
  person ran 3 minutes with no loss.
- **Hold and resume** are Teams' renegotiations: `call/mediaRenegotiation/` pushes `mediaNegotiation
  {mediaContent {blob, mediaLegId}, links {mediaAnswer}}`, answered by a `mediaAnswer` POST. **Teams
  renumbers the audio section's `a=mid` in such an offer**, which libwebrtc refuses ("The order of m-lines in
  subsequent offer doesn't match"): the offer goes to the sidecar (`Command::RemoteOffer`, `Event::LocalAnswer`)
  under the call's own `mid` (`renegotiation_offer`), and the answer goes back under Teams' `mid` with every
  section of its offer, refused ones at port 0 (`teams_answer`). A hold that was refused kept the microphone
  flowing to the other side. Android does the same through `remoteOffer`. **Teams' hold offer also gives
  extension id 3 another URI** (its `skype.com/.../fast_bandwidth_feedback`, where the call had
  transport-wide-cc), which libwebrtc refuses ("RTP extension ID reassignment not supported"; found live
  2026-11-14: Teams waited about 15 s for the answer, then dropped the media, and a resume stayed on hold).
  `renegotiation_offer` therefore fits the offer to the call's last plain SDP: an `a=extmap` whose id or URI
  that SDP binds otherwise is left out, so the answer declines it.
- **Ending**: the other side's `callEnd`/`conversationEnd` ends the voice session with `remote_ended`,
  90 s without an answer with `no_answer`. Neither is a join failure (`Ended::is_join_failure`), so the page
  does not open: it would ring them again.
  When no `callEnd` reaches this endpoint (seen once: the call went on until its media failed), the calling
  registration's `evt` 116 push naming this conversation (`cp.conversationController`, gzip) ends it too
  (`frames::conversation_over`, `same_conversation`), believed only 5 s after the answer since the push that
  follows an answer is not yet known to be 115 rather than 116. Android has no inflater for that gzip: it
  relies on `callEnd`.
- **Who speaks**: one mixed stream, so a person's own level is known only with one person in the call.
- **Checked live (2026-11-06)**, Microsoft's Test Call (`UserInitiatedTestCall`, `to: []`, then the bot
  `28:cf28171e-...` invited by `addParticipant`; it rings no one), a tone from `rv-voice`, through
  `Session::place_teams_call` (`examples/teams-test-call.rs`; the signalling alone in rv-teams
  `examples/teams-echo.rs`): token taken, answer received, DTLS up, the bot heard as decoded audio (its tile
  speaking and silent in turn), 0 packets lost each way at 10 ms, roster, leave. **A person rung the same
  day**, the GTK AppImage calling an account in Teams' web app in the same tenant, four short calls (5 to 15 s):
  the native window opened, a decline ended it with its message and no page, the user heard audio both ways on a
  real microphone and speakers, the name and speaking indicator were right, and hanging up from either side
  ended it; 0 packets lost, no fallback. A call past 35 s died until the broker lease was held; with it a call
ran 3 minutes (2026-11-07). The `a=mid` rewrite for hold and resume is in place but not yet run with a person. The media server connects before the callee answers, so `connected`
  comes while it rings (the window shows "waiting" until someone else is in the roster). **Not run**: hold and resume after the
  `a=mid` fix; a group chat's call; another network; SwiftUI (macOS CI only), Android on a
  device (Kotlin compiled only). Channels and meetings: below.
- **Answering an incoming call** (2026-11-11, from `teams.cloud.microsoft_call_in.har`): the ring
  push (`evt` 107, `gp`) carries the caller's offer (`callNotification.mediaContent.blob`, its
  `mediaLegId`), the `attach` link and the conversation's controller (`frames::Answering`, kept per
  room in `Session::teams_answerable`). Accept POSTs `attach {attach {requireMediaContent: false,
  links {end}}, additionalActions: [{name: join, url: <controller>, input {conversationRequest
  {roster, links}, participants {from}}}]}`, whose answer gives the call's links (`acceptance`,
  `subscribe`) and the conversation's (`leave`, `updateEndpointMetadata`, `calling::attached`); the
  sidecar answers the offer in answering mode (`ConnectExternal {answering: true}`, then
  `RemoteOffer`), and `acceptance` takes `callAcceptance {acceptedBy, acceptedCallModalities: [Audio],
  links, mediaContent {blob, mediaLegId}}` (answered by `callAcceptanceAcknowledgement`). The call
  then runs as a placed one (`drive` with the answer's mid: lease, metadata, renegotiations, end).
  Teams' web client also posts `progress {status: ringing}`, `updateEndpointState` and
  `updateParticipantProperties`: not sent, and **not yet run live**. A ring without the offer, or
  any failure, keeps the old path (Teams' page). Android does the same in `provider-teams`
  (`answerExternalCall`, `connectExternal(..., answering)`); SwiftUI from the foreground prompt only
  (a background account's ring still opens the page). `callKeepAliveInterval` (2700 s) is
  unexplained: no capture shows what it wants, so a call past 45 minutes may end.
- **Channels and meeting chats** (2026-11-18, from `teams.cloud.microsoft_channelmeeting.har` and
  `teams.cloud.microsoft_calendarmeeting.har`): their call is the thread's own, joined rather than rung
  (`TeamsSync::thread_call`, Android `joinsInRoom`), from the call button and a call card's Join alike.
  The thread's document (`threads/<id>?threadProperties=1`) names it: a running call in
  `awareness_conversationLiveState:<n>` (a JSON string, `status: Active`, `meetingInfo {organizerId,
  tenantId}`, else `groupCallInitiator`), else the meeting chat's `meeting` (`organizerId`, `tenantId`,
  `meetingJoinUrl` whose last segment is the message, `0` for a Meet now); `calling::meeting_join`.
  **Joining** is two POSTs, as Teams' web client does: `epconv` with `conversationRequest` (no
  `callInvitation`), `groupChat {threadId, messageId}` and `meetingInfo {tenantId, organizerId}` resolves
  the conversation (`join_meeting`, answering its `conversationController`), then that controller takes the
  same join with `participants.to: []` and the `callInvitation` (`join_meeting_call`), answering the call's
  links (`subscribe`, `leave`, `updateEndpointMetadata`); the call then runs as a placed one. A channel with
  no running call **starts** one in a single `epconv`: `conversationType: huddleGroupCall`,
  `suppressDialout: true`, `groupChat {threadId, messageId: null}`, `to: []` (Teams' web client also lists
  the channel's people with `invitationType: nudge`; not sent). A channel's call is keyed `messageId: 0`:
  Teams' own rejoin used it. A meeting chat with neither, or any failure, keeps Teams' page. **Not yet run
  live**: whether the meeting's media server answers by `call/acceptance/` as a person does, a lobby, the
  roster of a large meeting.

### Long calls: what ended them, and recovery (2026-11-05)

Two Rocket.Chat calls on meet.barrut.me closed by themselves after about an hour and a half. Nothing
was logged, so the exact trigger of those two is not known; the code showed why any trigger closed
the window without a word: the sidecar ended the call on the first of three events (the XMPP socket
gone, `PeerConnectionState::Failed`, a Jicofo `session-terminate`), rv-call ignored a Jicofo
`transport-replace` (its ICE restart or bridge move, which Jicofo then ends the participant for),
and every window closed on `snapshot.ended` whatever the reason. Now:
- **rv-call** (`jitsi/client.rs`): a socket with no frame for 90 s (`SILENCE`; pings every 25 s,
  `PING_EVERY`, are answered) is lost, even when TCP has not said so; a `transport-replace` from the
  focus ends the session at once (`transport_replaced`); `RV_CALL_DEBUG=1` prints each Jingle action
  and why a session ended.
- **rv-voice** (`call.rs`): `run` takes a joiner, not a join; a session lost on the way
  (`Stop::Lost`) is joined again with a new peer connection after 1, 2, 4, 8, 15 s
  (`REJOIN_DELAYS`; the count resets once a session connected), the window showing `Reconnecting`,
  the microphone and deafen choices kept and told to the room again. Commands still work between
  tries; hanging up stops at once. Only when every try failed is `Disconnected` sent, with the last
  reason. `RV_CALL_DEBUG=1` prints the peer connection's states.
- **Android** (`JitsiCall.kt`): the same loop in `JitsiSession.run`, a media `FAILED` recorded as
  the session's loss rather than ending the call, `reconnecting` read by `VoiceEngine` as the
  `reconnecting` state; only a first join that fails switches to the meeting's page.
- **Windows**: GTK `native_call.rs`, SwiftUI `NativeCallWindow.swift` and Android's call screen show
  "Reconnecting…" and, when a call ends other than by hanging up here, keep the window with "Call
  ended: <reason>" (`Ended::explained`, rv-core; rv-ffi `JitsiCallState.ended_reason`) and a Close
  button.
- **Evidence**: a 105-minute probe (two sidecars sending a tone, `RV_CALL_DEBUG=1`, a test room on
  meet.barrut.me) to see what the server does on a long call; see its result below.
- **The window that closed on long calls (2026-11-06)**: Jicofo ends the Jingle session of a participant
  alone in the room (`single-participant-timeout`, about 20 s) with a `session-terminate`, and offers a
  new session when someone joins. rv-call took that for the end of the call (`Ended("terminated")`), so
  the shipped 0.30.2 window closed: reproduced on meet.barrut.me with its own sidecar (B leaves, A
  `disconnected: terminated` 28 s later). The Jitsi web client stays in the room. Now rv-call emits
  `Event::MediaEnded`: the sidecar and Android close the peer connection and wait in the room for the next
  offer.
- **The socket lost during a long call (2026-11-06)**: two 0.30.2 sidecars in a test room, both staying:
  at 1 h 37 min 49 s one lost its signalling socket (`connection_lost`) and ended, the other one, alone,
  got `terminated` 20 s later and ended too. That is the reported 1 h 30. meet.barrut.me is behind
  Cloudflare, the other socket lived on on the same machine, and the media does not go through that socket:
  losing it must not end a call. Now it rejoins: a real socket shut down mid-call (`shutdown` on the
  sidecar's own descriptor) gave `reconnecting`, then connected and audio both ways 2 s later, with no
  `disconnected`. rv-call prints the socket's error to standard error.
- **A fresh factory per media session**: a second peer connection on a `PeerConnectionFactory` whose first
  connection closed gets the remote tracks but no audio frames (reproduced: A level 0.0 for C while C heard
  A; with a new factory A heard C at 0.8). `call.rs` `media()` makes the factory and the microphone track
  anew for each session, after `MediaEnded` and after a lost session alike. Android already rebuilt its
  factory (`dropMedia`).
- **Traces**: `rv_core::voice::set_log` (GTK `crashlog::dir()/voice.log`, rv-ffi `<cache>/voice.log`, kept
  under 4 MiB with one `voice.previous.log`) gives the sidecar its `RV_VOICE_LOG`; `call: lost`, `call:
  ended (reason)`, `call: media session ended` and the controller's `session ended` land there.

The windows are built from the Vibez voice page's parts: GTK `chat::voice` exports `controls`,
`avatar_card`, `avatar_frame`, `name_tag` and `Occupant`; SwiftUI `VoiceTileFace`,
`VoiceControlButton`, `VoiceHangUpButton`; Android `VoiceTile`, `VoiceCard`, `ControlButton` in
`ui/voice.tsx`. No photos (Jitsi occupants are not accounts): the initial. Devices: GTK's microphone
menu lists inputs and outputs (`VoiceController::devices`); SwiftUI and Android have no device menu
in a native call yet.

### Mattermost Calls plugin, audio (2026-11-11)

Self-hosted Mattermost's calls are the `com.mattermost.calls` plugin (v1.12.3 on the bench), an
SFU (rtcd) inside the server. Learnt from the plugin's compiled web bundle (its compiled builds
are MIT; the source is AGPL, read for the protocol only, never copied) and from rtcd's source,
then run on the bench. kChat has no such plugin: its calls stay kMeet's.

- **Signalling** (rv-call `mattermost`): the client opens **its own** `/api/v4/websocket`
  (`authentication_challenge`), then `custom_com.mattermost.calls_join {channelID, title, threadID}`;
  per-connection events carry `data.connID` (`_join` ack, `_error` refusal such as "insufficient
  permissions" when the channel's calls are off, `_signal` with `data.data` the JSON `{type, sdp |
  candidate}`); channel events (`_call_state` with `call` as a JSON string of sessions,
  `_user_joined`, `_user_left`, `_user_muted` / `_user_unmuted`, `_user_voice_on` / `_off`,
  `_call_end`). The client offers; **the SDP goes out as MessagePack** `{action: ..._sdp, seq,
  data: {data: <zlib of {type, sdp} JSON>}}` in a binary frame; candidates as `_ice {data: <JSON>}`.
  `_mute` / `_unmute` / `_leave` act. A session starts muted: the adapter unmutes once its offer
  went out unless the user muted.
- **The SFU forwards no one's audio until the client's data channel is open** (rtcd
  `handleDCNegotiation` waits on `dcOpenCh` before `handleTracks`): the offer opens one
  (`calls-dc`), which `CallSignaling::data_channel` asks the engine for (sidecar `ensure_pc`, Kotlin
  `peer()`). Without it the call connected but stayed silent both ways. SDP over the channel
  (`EnableDCSignaling`) and its lock messages are not used: the server takes its own lock.
- **Tracks** carry the sender's session id as their stream id, so `Member.id` is the session id
  and who speaks comes from the audio itself; names come from `GET /plugins/com.mattermost.calls/<channel>`
  (`call.sessions[].{session_id, user_id}`), read by the apps (rv-core `ensure_plugin_call_people`,
  the mobile `pluginCalls.personName`).
- **REST** (rv-core `mattermost/calls.rs`, mobile `pluginCalls.ts`): `/version` says the plugin
  runs, `/<channel>` whether its calls are `enabled` (else `/config` `DefaultEnabled`), `/config`
  the `ICEServersConfigs`, `/turn-credentials` when `NeedsTURNCredentials`. An unlicensed server has
  `GroupCallsAllowed: false`: direct messages only. Its `custom_calls` post (`start_at`, `end_at`)
  is the call card, `mmcalls:<channel>` its call id; a DM's running one rings like kChat's.
- **Checked live (2026-11-11)**, two `rv-voice` sidecars as `bob` and `rvadmin` in their DM on the
  bench, each with a tone: joined, the roster right, both tones received (0 lost), speaking shown
  from the audio, mute and unmute seen by the other side. **Not run**: the GTK and SwiftUI windows,
  Android, a channel call on a licensed server, TURN, ringing (off on the bench).
- On this machine the sidecar hung creating libwebrtc's factory while the GPU driver was stuck
  (`drm_sched_entity_flush`, the video encoders' probe): the test ran with `/dev/dri` hidden
  (`unshare -rm`). A machine in that state cannot start any native call.

### Video in native calls (2026-11-12)

- **rv-call** names video: `Event::Videos` (the whole list of remote cameras and screens, by track or
  stream id, whose, which kind), `can_send(kind)`, `video_stream(kind)` (the msid stream id of this
  side's camera or screen track), `set_video(LocalVideo)` (told after a track was attached or
  detached; the adapter renegotiates as its provider does), `offers_local_sections` (Jitsi: a local
  track goes on the first unused transceiver, which its offer's own section created; otherwise a
  fresh `SendOnly` transceiver, never one the server opened for someone else's track, which libwebrtc
  then refused to answer), `video_of(track, stream)` (a provider naming tracks in their ids),
  `data_channel` with `data_protocol` and `Event::DataMessage`.
- **Sidecar** (`call.rs` `Videos`, `sync_video`): the camera and screen captures of `video.rs` (the
  LiveKit path's) become tracks of the call's peer connection, made anew for each media session's
  factory; remote video tracks go to the app's frame stream (`video::watch_rtc`) under the member's id,
  this side's previews under `frames::LOCAL_PREVIEW`; `Event::CallVideo` says what the provider takes
  (`Snapshot::call_video`). A renegotiation offer goes out at once on a connected call; when the
  server's offer crosses it, this side rolls its own back, answers, offers again, and an answer it
  cannot set is answered by offering again (at most 3) rather than by rejoining.
- **Jitsi and kMeet**: the client announces `rtp:video`, `receive-multiple-video-streams`,
  `http://jitsi.org/tcc`, `urn:ietf:rfc:4588` (rtx) and `dtls-sctp`. Without transport-cc Jicofo offers
  no bandwidth feedback, and **without a bridge channel the bridge forwards no video at all**:
  meet.barrut.me offers neither SCTP nor a colibri WebSocket, and Jitsi's web client adds a `data`
  content to its `session-accept` itself (captured 2026-11-12); the client does the same (an
  `m=application` section in the synthetic offer, the data channel "JVB data channel", protocol
  `http://jitsi.org/protocols/colibri`) and sends `ReceiverVideoConstraints` (720 lines a camera, 1080
  a screen) over it, or over the bridge's WebSocket when one is offered. Each remote camera or screen is
  a receive section (`sdp.rs` `Stream`: a source name's SSRCs and groups; a simulcast camera received as
  its first layer and that layer's rtx, which the bridge forwards every layer under); this side's
  camera and screen go on two sections of their own (`video`, `screen`, VP8 first) and reach the focus
  as `source-add` / `source-remove` (`<endpoint>-v0` camera, `-v1` desktop, with `videoType`), read from
  libwebrtc's answer by their msid stream; `SourceInfo` in the presence says which are muted, and a
  muted remote camera is dropped from `Videos`.
- **Mattermost Calls plugin**: `/config` `AllowScreenSharing` and `EnableVideo` say what a member may
  send (`MattermostCall::screen`, `camera`); `screen_on {screenStreamID}` / `video_on {videoStreamID}` go
  before the offer carrying the track, `screen_off` / `video_off` after; the SFU names a forwarded track
  `<screen|video>_<sender session>_<random>` (`mattermost::video_of`). The plugin's web client learns the
  kind from a `MediaMap` data-channel message instead, not read here.
- **Checked live (2026-11-12)**: two sidecars (`RV_VOICE_FAKE_VIDEO=pattern`) on meet.barrut.me each
  decoded the other's camera (640x480) and screen (1280x720); Jitsi's web client in headless Chromium
  decoded the sidecar's camera and screen, and the sidecar its simulcast camera; on the Mattermost bench
  (`EnableVideo` off) screens both ways, a camera only in fits (the server does not carry them with
  video off). The GTK window against a real Jitsi call (`RV_SMOKE_NATIVE_CALL`, Fedora image under
  Xvfb): the peer's screen on the stage, both cameras in tiles. On this machine the sidecar's libwebrtc
  hangs creating its video codecs while `/dev/dri` is reachable (the stuck GPU driver above): tests ran
  with it hidden.
- **Windows**: GTK `native_call.rs` (camera and screen buttons where the provider takes them, a camera
  in its tile, a shared screen, someone else's first, on a stage above the people); SwiftUI
  `NativeCallWindow.swift` over rv-ffi `JitsiCallSession` (`frame`, `set_camera`, screen share; no camera
  button, the sidecar capturing no camera on macOS), macOS CI build only.
- **The room's chat beside the call**: GTK `call_chat.rs` (the room's latest messages from the store,
  redrawn on its changes, and a composer posting through the outbox) behind a header button
  (`native_call::Opened::chat`); SwiftUI the room's `MessageList` and `Composer` on a `RoomModel` of
  its own (`AppModel::callChat`).
- **Android** (`JitsiCall.kt`, `NativeVideo.kt`, rv-voice-mobile's `local` event and `video`/`videoOf`
  calls): `JitsiSession` sends the front camera (Camera2) and the screen (media projection, `VoiceService`
  taking that foreground type while a native call shares), the captures outliving a media session; a
  track goes on the first unused section where the provider's offers carry this side's (Jitsi), else on a
  send-only section of its own; the data channel opens with its protocol, its messages waiting for it;
  crossing offers handled as on desktop. Remote videos are named from the call's list, else from the
  track's own ids, else shown as their stream's camera until the list comes. The call screen shows a
  camera in its tile, a shared screen on the stage with the people in a strip (`ScreenStage`, shared with
  the Vibez voice screen), camera and screen buttons only where the call can send them, and a chat button
  opening the call's room over the call. Checked by compiling only (Kotlin, TypeScript, rv-voice-mobile
  tests), not run on a device.
- **Owed**: Chime on Android (this side's screen share, the `#content` socket); the plugin's `MediaMap`. WhatsApp's
  video has its own path, below; Teams' is after it.

### WhatsApp 1:1 calls, audio (2026-11-11)

Unlike every other provider, WhatsApp's call is not WebRTC the sidecar can join: whatsapp-rust
(vendored, `crates/rv-whatsapp`) speaks it whole, signalling (`<offer>`, `<accept>`, the callKey
encrypted per device), the relay (one UDP socket, DTLS, SCTP and a data channel, `voip-relay-native`)
and Meta's MLow codec (`voip-mlow`, pure Rust; `voip-libopus` is left out, its C build not wired,
so a peer's standard Opus or CELT escape frames are not decoded). Its audio is PCM, 16 kHz mono in
60 ms frames of 960 samples (`AudioSource` / `AudioSink` channels).

- **The engine** (`engine/src/calls.rs`) keeps each ringing call's `IncomingCall` (`Ringing.incoming`),
  answers (`accept_call`) or places (`place_call`) with PCM channels, and serves them on a TCP socket of
  its own on 127.0.0.1 (a random port, a token line first, then 1920-byte frames both ways), so the
  app's audio engine carries the sound whichever process holds the engine: rv-core's thread on desktop,
  the app's own process through JNA on Android. Its `call` frames say `placed` (the real call id),
  `connecting` (with `host:port#token`), `active` (media connected) and `ended`.
- **Desktop**: `WaSession::carry_call` sends the command, waits for `connecting`, starts the sidecar on
  it (`Command::ConnectPcm`), names the peer (`RemoteRoster`) and follows the frames, the microphone
  (`mute_call`) and the hang-up (`hangup_call`). The sidecar (`call::run_pcm`) taps the frames the room
  would hear (echo cancelled, denoised, `Mix::set_tap`), averages them down to 16 kHz and sends a frame
  every 60 ms (silence while muted); the far side's frames are interpolated to 48 kHz stereo and fed to
  the mix 10 ms at a time behind an 80 ms cushion. Checked with an echo server: one frame every 60 ms,
  the echoed tone heard and shown speaking.
- **Android**: the provider's `carryWhatsAppCall` and the voice module's PCM mode
  (`JitsiSession.startPcm`: `AudioRecord` `VOICE_COMMUNICATION` with `AcousticEchoCanceler` and
  `NoiseSuppressor`, `AudioTrack` `USAGE_VOICE_COMMUNICATION`, 16 kHz). The library cross-compiles for
  arm64 with the call features.
- **SwiftUI** (2026-11-12): the same `carry_call` through rv-ffi (`jitsi_call::carry_whatsapp`,
  `WhatsAppChat.answerCall` / `startCall`), shown in `NativeCallWindow`; background accounts ring
  through `background.rs` `rings`, whose `phone_only` keeps "Answer on the phone".
- **Not run**: a call with a phone, either way, on any app.
  Group calls (built in the library) are not offered.

### WhatsApp 1:1 calls, video (2026-11-21)

whatsapp-rust carries video as H.264 access units (Annex-B, Constrained Baseline, RFC 6184 packets,
FEC, PLI) and leaves the codec to its consumer (`src/voip/video.rs`). In a 1:1 call there is one
picture each way: the library's screen-share signalling is for group calls only, so a shared screen
goes out as this side's video.

- **The engine** (`engine/src/call_video.rs`): the call's socket takes a second connection,
  `<token> video` as its first line (the audio one stays the bare token), then messages of a kind byte,
  a little-endian `u32` length and a payload. From the app: a picture (`u32` 90 kHz timestamp, one
  access unit), sending on or off, a keyframe request. To the app: the peer's picture (keyframe and
  orientation bytes, timestamp, access unit), the peer sending on or off, a keyframe request, a refused
  switch. A video call is answered with the library's video endpoints, then this side's direction is
  stopped; the peer's switch to video (`UpgradeRequestV2`) is accepted the same way, so their picture
  shows while the camera stays off. Sending on resumes this side's direction (`resume_video`) or asks the
  peer to switch (`start_video`, state 11); a refusal or a lapse (`UpgradeReject`, `RejectByTimeout`)
  comes back as refused. The call frames and rv-core are unchanged: the app knows the socket already.
- **Desktop** (`voice/src/pcm_video.rs`, `h264.rs`): `run_pcm` opens the video connection and says
  `CallVideo {camera, screen}`; the captures of `video.rs` hand their pictures to a tap (`Capture::tap`),
  the screen's while shared, else the camera's, encoded at up to 1280x720 and 15 a second (0.8 Mb/s a
  camera, 1.2 a screen, a keyframe every 3 s and on request). The codecs are libwebrtc's own: its
  static library links OpenH264 2.6's encoder and FFmpeg's H.264 decoder on Linux, Windows and macOS
  (not Android's), called directly with their structs laid out from the shipped headers (sizes checked at
  compile time). The peer's pictures are decoded, turned by their orientation (quarter turns, clockwise)
  and shown as their camera; a failing decoder asks for a keyframe at most once a second.
  `video_declined` turns the captures off and rv-core's camera and screen wishes with them.
- **SwiftUI**: the same sidecar through rv-ffi's `JitsiCallSession`, shown in `NativeCallWindow` (the
  peer's camera in their tile, the screen button); `WhatsAppChat::call_answerable` and `background.rs`
  `phone_only` send only a group's call to the phone. No camera goes out: the sidecar captures none on
  macOS. Checked by the macOS CI build only.
- **Android** (`modules/voice/android/.../PcmVideo.kt`, `JitsiCall.kt`): `JitsiSession.runPcm` opens the
  video connection beside the audio (`PcmVideo.open`, WebRTC's native library loaded first for its
  buffers) and only then says the call takes camera and screen (`canCamera`, `canScreen`), so the call
  screen shows its buttons and tiles as for any native call. The captures of `NativeVideo.kt` are tapped
  (the screen while shared, else the camera): one picture every 66 ms, cropped and scaled within
  1280x720 (720x1280 standing) in whole macroblocks, turned upright (`YuvHelper.I420Rotate`), then
  `MediaCodec`'s H.264 encoder (`H264Encoder`: `COLOR_FormatYUV420Flexible` through `getInputImage`,
  Constrained Baseline where the codec lists it else Baseline, level 3.1, 0.8 Mb/s a camera, 1.2 a
  screen, 15 a second, a keyframe every 3 s and on `PARAMETER_KEY_REQUEST_SYNC_FRAME`); its
  `BUFFER_FLAG_CODEC_CONFIG` output (SPS and PPS, Annex-B) is put before each keyframe that lacks them
  (`PcmVideoWire.accessUnit`). The peer's units go from the first keyframe to `MediaCodec`'s decoder
  (`H264Decoder`, low latency from Android 11) rendering onto a `SurfaceTextureHelper`'s surface, its
  orientation as the frames' rotation (`setFrameRotation`), the frames shown under the peer's roster id
  as their camera (`DecodedFeed`); a lost unit or a codec error asks for a keyframe at most once a
  second. A refusal stops both captures and emits `videoDeclined`, which the call screen shows as a
  notice. The framing, parameter sets, fit and clock are unit-tested (`PcmVideoWireTest`).
- **Checked** (2026-11-21): the encoder's output decoded back at its size (`h264` tests); the real
  sidecar against a stand-in engine echoing its pictures as the peer's (`~/.cache/rvtest/pcm-harness.py`,
  outside the repo): about 15 pictures a second out, sending on then off, the echo decoded, turned and
  shown under the peer's id. **Not run with a phone**: the orientation's direction, and what a phone
  makes of the stream, are unverified. Android: checked by compiling (Kotlin, an x86_64 debug build) and
  the framing's unit tests; not run on a device, against an engine or with a phone.

### Teams call video (2026-11-23)

- **What Teams does** (the 2026-11-02 HARs, and Teams' web calling bundle `calling-pluginless-*.js` the
  main HAR holds): a call's offer has `main-audio`, `main-video` (the cameras), `applicationsharing-video`
  (a shared screen) and the `data` (`m=x-data`, SCTP) sections, named by `a=label`. Teams' web client
  rebuilds its browser's SDP by label and renumbers the `a=mid`s between offers (its share went from mid 2
  to 4, the other side's to 5). In a 1:1 call (not `multiParty`), a camera or a screen starting is a
  renegotiation: the side starting it posts `renegotiate {mediaNegotiation {callModalities, sender, links
  {mediaAnswer, rejection}, mediaContent {blob, mediaLegId, negotiationTag}}}` (the tag is
  `<participantId>;v_1` for a camera, `;ss_1` for a screen), Teams answers on the `mediaAnswer` callback,
  and the answer is acknowledged by a POST on its `links.mediaAcknowledgement` (the web client's is empty, this client sends `{}`) (a `callAcceptance`'s
  is `links.acknowledgement`); the other side's renegotiation comes as `mediaNegotiation` (the hold path
  above), answered on its `links.mediaAnswer`, and Teams then pushes `mediaAcknowledgement`. The `renegotiate` link is `links.mediaRenegotiation` of a
  pushed `callAcceptance` on a placed call, and `callAcceptanceAcknowledgement.links.mediaRenegotiation`
  of the acceptance's own answer on a call answered here (`call_in` HAR). A call through Teams' media
  server (`multiParty`: group calls, meetings) sends no one's video until the client asks for a source
  (`controlVideoStreaming {controlInfo {sourceId, streamMsid, fmtParams}}`, over the data channel or the
  signalling), and Microslop's RTCP video source request (`PSFB` `AFB` type 1) is the native clients' form
  of it: the data channel's form is built (below), the RTCP one is not.
- **What a 1:1 call between two web clients showed** (the 2026-11-23 `video1` HAR): Teams never reuses a
  mid. Its first offer lists `applicationsharing-video` open under mid 2 (`inactive`); the web client
  answers it with port 0 and no mid, and its shares later took mids 6 and 5 (data being 3). A stopped share
  goes back to the placeholder `m=video 0 RTP/SAVP 34` with only its label, no mid; a camera turned off
  keeps `main-video` under its mid, `inactive`, its msid and sources kept. Video is H.264 (several
  profiles) and AV1 only, no VP8 or VP9. `callModalities` say `Audio` plus `Video` while the camera is on,
  `ScreenSharer` while sharing; the answers say `audio`, `screenViewer`.
- **What a group call showed** (the `video2` HAR, no websocket frames; the `video3` meeting the same, with
  its Trouter frames): the client's own offer lists ten `main-video` sections (mid 1 its camera with a range
  of 100 sources, 2 to 6 `sendrecv`, 7 to 10 `recvonly`), `applicationsharing-video` mid 11 and `data` mid
  12. Before any camera is on, mid 1 and 11 are already `sendrecv` transceivers with no track (`a=msid:-
  <track>`, sources and `FID` groups listed; mid 1's source line is Teams' `fake_attribute`), and the
  invitation's `mediaContent` carries `mediaDescriptions {descriptions: [mids 1 to 6 and 11, each
  recvonly], requestId: 1}` (the `recvonly` sections 7 to 10, audio and data are never listed) and
  `applyChannelParameters {multiChannelParameter {mids: ["*"], mediaParameter: "{sendSideBWSeed}"}}`. The
  media server answers every one of them `sendrecv` (no direction line), with its own range and
  `x-source-streamid` (mid 1 is 202, mid 11 is 212). The pushed `callAcceptance` (gzip, `call/acceptance/`)
  carries the links: `updateMediaDescriptions` (`{cc}/cc/v1/active/<call>/.../updateMediaDescriptions`,
  through the flight proxy), `applyChannelParameters` and `controlVideoStreaming` (`{cc}/cc/v1/mcProxy/...`).
  Turning the camera or a share on or off is no renegotiation: the client posts `updateMediaDescriptions
  {UpdateMediaDescriptions {mediaDescriptions {descriptions [{mid, direction, label?}], negotiationTag,
  requestId}}}` with the same seven mids, `sendrecv` and `label: main-video` on mid 1 while the camera is
  on, `sendonly` and `label: applicationsharing-video` on mid 11 while sharing, `recvonly` and no label
  otherwise; the tag is `<participantId>;v_1` for a camera change, `;ss_1` for a share's, and `requestId`
  counts on from the invitation's 1. Teams answers 200 `{code: 0, phrase: "Success."}` and pushes nothing
  for it but the next `rosterUpdate`, where this side's own endpoint shows `main-video` 202 or
  `applicationsharing-video` 212 `sendrecv` / `sendonly`, then `recvonly` again. Just before its camera
  first goes out (once a call) the client posts `applyChannelParameters {multiChannelParameter {mids:
  ["1"], mediaParameter: "{maxVideoSendCapabilities {caps {max-width 1280, max-height 1280, max-fps 30,
  max-streams 3, max-layers 3, sequence-number}}}"}}` (202, empty), and before a share starts
  `conv/<id>/updateParticipantProperties` with `screenSharerAttributes {sequenceNumber, value {source:
  1}}` on its own account (202). On the data channel, once this side's camera or share goes out, the
  server sends `vdclc {sourceId: <its section's stream id>, fmtParamsList: ["KeyFrame=1;max-br=..;
  max-fps=..;max-fs=..;rid=..;ssrc=.."]}` (what it wants of that stream, the first with a keyframe, the
  camera's at 320x180 for a small tile), again as that changes, and the client sends `ssbwe {BwMessage
  [{streamMsid, bw}]}`.
- **What a meeting's data channel showed** (the 2026-11-24 `video3` HAR with its socket frames, and
  `teams-datachannel.json` from a script wrapping `RTCPeerConnection.createDataChannel`, two accounts):
  the browser opens one in-band channel `main-channel` (id 0, no protocol) for the `x-data` section, which
  is a plain SCTP channel (`a=x-data-protocol:sctp`, no `sctp-port`). The media server's answer names every
  section by `a=x-source-streamid` (201 for mid 0 to 213 for the data, in m-line order), and each
  `rosterUpdate` push lists an endpoint's `call.mediaStreams` with its own ids the same way (`main-audio`
  414, `main-video` 415, `applicationsharing-video` 425, `data` 426) and `direction` `sendrecv` while that
  video is on, `recvonly` while off. Every message is a frame: `0x10`, the header's remaining length
  (`0x0f`, `0x13` for two receivers), the kind (`0x92` JSON; `0x80`, `0x81` carry the client's own events
  and a handshake with the other endpoint, not needed), `0x00`, a little-endian frame counter, the
  sender's stream id, the receivers' count and ids (big-endian; the server is `0xfffffffc`), `0x01`, then
  a JSON array. The client sends `syn {client_capabilities}` (answered `ack`), a `heartbeat {state: 0}`
  every second as the server does, and, a second after a remote video turns `sendrecv`, `sr
  {controlVideoStreaming {sequenceNumber, controlInfo {sourceId: 415, streamMsid: 202, fmtParams
  [{max-fs, max-mbps, max-fps, profile-level-id: 64001f}]}}}` putting that camera on its mid 1; a share
  went on its `applicationsharing-video` section (212), and `sourceId: -1` when the video stopped. The
  server answers `sr_res {result: ok, sequenceNumber}`, and sends `bwe`, `dsh` (dominant speakers' audio
  ids) and `vdclc` (what it wants of this side's own sending, a keyframe flag included).
- **The translation** (rv-teams `sdp.rs`): a `Layout` per call holds the plain sections libwebrtc
  negotiated, in their order. The audio keeps its section whatever mid Teams gives it (as before); a video
  section keeps its plain mid equal to Teams' own, since the other side's packets carry it (`sdes:mid`), so a
  video section under a mid the call has not seen becomes a new plain section after the others and the
  section Teams stopped listing is refused (port 0); `m=x-data` is refused, as Microslop does. Every
  section of Teams' libwebrtc takes no media on (the data section, a refused one) stays in the plain offer
  refused under its mid (`m=application 0` for the data), so libwebrtc never gives this side's new screen
  section a mid Teams already uses: before, the share took mid 3, Teams' data section. Each video
  section the other side sends gets `a=msid:teams-<mid>` (its own msid and `a=ssrc` msids dropped) and is
  named camera or screen by its label. This side's offer is labelled by the stream ids the sidecar gives
  its tracks (`<endpoint>-camera`, `<endpoint>-screen`); a section of its own takes the label over from one
  that only received, and a capture stopped is told as `recvonly` or `inactive`, its track gone.
  `modalities` fills `callModalities` (`Video`, `ScreenSharer`, `ScreenViewer`) in the accept, the answers
  and `renegotiate`. libwebrtc's `m=application` data section goes out as `m=x-data ... 127` with
  `x-data-protocol:sctp` and a made-up source range, and Teams' comes back as `UDP/DTLS/SCTP
  webrtc-datachannel` on port 5000; the stream id of each section is kept (`video_slots`, `data_stream`). A
  group call's `Layout::group` offers every receiving video section rather than one per label, the last one
  not labelled yet as the shared screen's; a receive-only section names the range `1-1`, as the web
  client's do.
- **The sidecar** (`call.rs` `External`, `ConnectExternal {video}`): it takes the camera and the screen;
  the camera goes on the first unused video section (the one receiving the other side's camera,
  `reuses_section`), the screen on a new send-only section, as the web client's share does. Each start or stop
  is offered anew (`set_video` asks for `OfferWanted`), and an offer waits for the last one's answer
  (`offers_in_turn`, `Call::deferred`). Its offers receive video (`offers_to_receive_video`): LiveKit's
  binding passes libwebrtc's legacy `offer_to_receive_video` as 0 unless told, which takes receiving out
  of every video section offered (the camera's section went send-only and the other side's camera
  stopped); with it, a placed call's first offer also carries a receive-only camera section, as the web
  client's does. An offer of Teams' crossing this side's rolls it back, answers, then
  offers again; a late answer to a new offer no longer ends a running call. `Command::RemoteVideos` names the
  other side's videos by stream, whose (the one other person in the roster) and which kind.
  `ConnectExternal {data_channel, receive_videos}` opens a data channel by its label and adds that many
  receive-only video sections (then the first of them is the camera's); `SendData` sends a binary message
  on it once open, and what arrives comes back as `Event::Data`. `ConnectExternal {video_up_front}`
  (`CallSignaling::video_up_front`) puts a camera and a screen section, both `sendrecv` with no track
  (`Videos::prepare`), in the first offer, before the receivers: a capture starting swaps its track in
  (`set_track`), a stop swaps in the track that sends nothing, and neither asks for an offer.
- **rv-core** (`teams_call.rs` `drive`): each new local offer (`VoiceController::local_offers`) goes through
  `renegotiate` once the call is answered; a `mediaAnswer` push answers it while it waits
  (`Layout::offering`), and an offer of Teams' meanwhile withdraws it. GTK and SwiftUI need no change: the
  window shows the camera and screen buttons from `CallVideo`, as for Jitsi. A channel or meeting call
  (`join_thread_call`) opens `main-channel` with five receiving sections (four cameras, one screen: this
  side's own camera and screen sections, offered up front, and three receive-only ones); once answered it
  sends `syn`, a heartbeat every second, and (rv-teams `datachannel.rs` `Routing`) asks for each roster
  source turning live on a free section of its kind and for nothing there when it stops, the videos then
  named after whose source each section carries. A fifth camera is not shown. This side's own camera and
  share there go as the web client's do: the group `Layout` keeps the own sections' directions as
  libwebrtc offers them and records the sections that may send; the invitation carries their
  `mediaDescriptions`, all `recvonly` (`calling::describe_media`); and once accepted, each change of what
  the sidecar sends (its own participant's `camera` / `screen`) is posted one at a time to the
  acceptance's `updateMediaDescriptions` link (`sdp::media_descriptions`, `sending_steps`), after
  `applyChannelParameters` before the camera first goes out (one stream, one layer, 1280x1280 at 30) and
  `updateParticipantProperties` before each share. A 1:1 call keeps `renegotiate`.
- **Checked** (2026-11-23): the translation against SDP shaped like the HARs' (camera and screen received,
  a renumbered screen section, this side's share taking the label over, a stopped camera), the external
  signalling's offers, and the controller's new offers and `RemoteVideos` against the fake sidecar. The real
  sidecar (`RV_VOICE_FAKE_VIDEO=pattern`) driven with Teams' web offer from `_call_out` made plain, the other
  side's camera turned on, through a script outside the repo: libwebrtc took every translated offer and
  answer, the other side's camera track came up under its stream and was shown on its member, the camera
  went out `sendrecv` on `main-video`, the screen on a new `applicationsharing-video` section, and each
  stop was offered anew as `inactive` (run again after the reserved mids on 2026-11-24: the share took mid 4
  and offered H.264 and AV1). That run also found that taking a stopped capture off its sender
  crashed the sidecar in every native call (webrtc-sys dereferences the track it is given): the sender now
  gets a track that sends nothing. **Not run
  against Teams**: a group call's channel and requests (the real sidecar's offer went through the
  translation and a Teams-shaped answer outside the repo: the data channel and five slots, stream ids
  202 to 207); this side's own camera or share in a group call (the up-front sections were checked on
  the real libwebrtc offer in the sidecar's tests, `sendrecv` with `msid`, sources and `FID` like the web
  client's, and the requests against the capture's shapes): whether the media server forwards this
  side's video without honouring `vdclc` (the sidecar ignores it: no keyframe on demand, no resolution
  asked, no `ssbwe`), whether `applyChannelParameters` matters to it, whether it takes a one-source range
  where the web client reserves 100, and the made-up `sequence-number` 1 of the camera's capabilities; a
  group chat's call (placed by `epconv` with its people) still goes the 1:1 way, no data channel; whether Teams takes `renegotiate` from this client, whether it minds a stopped share
  told as `inactive` on its mid where its web client goes back to the placeholder, whether Teams' native
  clients renegotiate as its web client does, and the codec the two sides settle on.
- **Android** (2026-11-23): `packages/provider-teams/src/calling.ts` ports `sdp.rs` (`Layout`, `teamsOffer`,
  `plainAnswer`, `plainOffer`, `teamsAnswer`, `remoteVideos`, `modalities`, with its unit tests in
  `calling.test.ts`) and `drive` (`runCall`: one push or offer at a time, an offer made before Teams took a
  placed call waiting for its acceptance); `ExternalCallEngine` (`packages/provider-api/src/calls.ts`) gains
  `offers`, `sending` and `videos`. The Kotlin call (`JitsiCall.kt` `startExternal(identity, answering, video)`)
  is the sidecar's `External`: camera on the first unused video section (`reused`), screen on a send-only
  transceiver of its own, streams `<identity>-camera` / `<identity>-screen`, a new offer per start or stop
  only from a stable connection (`offerInTurn`, `deferred`), Teams' offer crossing this side's rolling it back
  (`remoteOffer`), a refused first answer ending the call and a late one ignored (`remoteAnswer`), and
  `remoteVideos` feeding the naming the Jitsi path uses (`shown`). Its offers carry `OfferToReceiveVideo`
  (org.webrtc leaves video sections alone without it, where LiveKit's Rust binding defaulted to stripping
  their receiving), so a placed call offers a camera section from the start. The call screen
  (`app/call/[callId].tsx` `externalVideo`) opens the call with video and so shows the camera and screen
  buttons. The fixes the 2026-11-23 capture led to (the answer acknowledged on `mediaAcknowledgement`,
  `negotiationTag`, Teams' other mids reserved) are ported. Group calls (2026-11-24): `calling.ts` takes
  `Layout.group`, the x-data translation (`teamsData` / `plainData`), `streams`, `videoSlots` and
  `dataStream`, and `packages/provider-teams/src/datachannel.ts` ports `datachannel.rs` byte for byte (JSON
  keys in serde_json's sorted order); `joinThreadCall` starts the engine with `{dataChannel: 'main-channel',
  receiveVideos: 5}` and `runCall` greets, beats every second (cleared when the call ends) and routes as
  `drive` does. The Kotlin call adds the receive-only transceivers before its first offer (the camera's
  `addTrack` then reuses the first) and opens the channel ordered and in-band; `sendData` queues until it is
  open and what arrives reaches JS as the `data` event (bytes as a number array). **Checked**: compiled and
  unit-tested only, not run on a device nor against Teams. **Owed**: this side's camera and share in a
  group call still go through `renegotiate`, where the desktop now offers them up front and posts
  `updateMediaDescriptions`.

## Order

Jitsi first (open protocol, a Rust reference, the server offers, two providers at once;
chat.barrut.me's own Jitsi has no token and no TURN to fight), then Chime for Slack
(official open signalling, credentials already reach the client), then Teams 1:1 audio,
Teams meetings and video last.

## Captures still owed

- **kMeet**: one live call in `#testtest` from the kChat app itself (the `/conferences`
  answer's `url` shape, the XMPP `session-initiate`, the TURN servers from XEP-0215).
- **Slack**: nothing blocking (the second capture has credentials, signalling, TURN and SDP, the
  third cameras and screen shares). Useful later: the main socket's `LEAVE`. A huddle opens in its own
  window: Chrome's DevTools setting "Auto-open DevTools for popups" catches it from the start.
- **Teams**: what `callKeepAliveInterval` wants of a call past 45 minutes; the
  `mediaAnswer` push answering this side's `renegotiate`; the pushes of a meeting join (the HARs of
  2026-11-18 hold its requests, not the socket). The outgoing call's answer was read live
  (2026-11-06), the auth audience, its creating request and screen sharing captured (2026-11-02),
  a meeting join and a channel call (2026-11-18), a meeting's data channel and roster sources
  (2026-11-24).

## Sources

- apps/desktop/voice/Cargo.toml, apps/desktop/voice/src/audio.rs
- apps/mobile/modules/voice, crates/rv-voice-mobile
- crates/rv-voice-protocol
- docs/MATTERMOST.md (kMeet conferences)
- apps/desktop/voice/src/call.rs, apps/desktop/voice/src/video.rs, crates/rv-call/src/jitsi/sdp.rs,
  apps/desktop/crates/rv-gtk/src/native_call.rs, apps/desktop/crates/rv-gtk/src/call_chat.rs,
  apps/desktop/macos/Sources/RocketVibe/NativeCallWindow.swift
- apps/mobile/modules/voice/android/src/main/java/com/rocketvibe/voice/NativeVideo.kt,
  apps/mobile/app/call/[callId].tsx, crates/rv-voice-mobile/src/call.rs
- apps/mobile/modules/voice/android/src/main/java/com/rocketvibe/voice/PcmVideo.kt,
  apps/mobile/modules/voice/android/src/test/java/com/rocketvibe/voice/PcmVideoWireTest.kt,
  crates/rv-whatsapp/engine/src/call_video.rs
- crates/rv-call/src/mattermost, apps/desktop/crates/rv-core/src/mattermost/calls.rs,
  apps/mobile/providers/mattermost/pluginCalls.ts
- The Calls plugin 1.12.3 (`mattermost-plugin-calls-v1.12.3-linux-amd64.tar.gz`, its webapp
  bundle) and https://github.com/mattermost/rtcd (`service/rtc/sfu.go`, `session.go`, `dc.go`)
- brain/features/calls.md, brain/architecture/teams.md, brain/architecture/slack-session.md
- crates/rv-teams/examples/teams-ic3.rs (ic3 token probe), crates/rv-teams/examples/teams-foci.rs
- crates/rv-teams/src/sdp.rs, crates/rv-teams/src/calling.rs, apps/desktop/crates/rv-core/src/teams_call.rs
  (Teams' video), Teams' web calling bundle `calling-pluginless-*.js` in `teams.cloud.microsoft.har`,
  `teams.cloud.microsoft_screenshare.har`, Microslop's `crates/ost-microsoft/src/calling/rtcp.rs`
- packages/provider-teams/src/calling.ts, packages/provider-teams/src/calling.test.ts,
  packages/provider-teams/src/datachannel.ts, packages/provider-teams/src/datachannel.test.ts,
  packages/provider-api/src/calls.ts, apps/mobile/modules/voice/android/src/main/java/com/rocketvibe/voice/JitsiCall.kt
  (Teams' video on Android)
- crates/rv-teams/src/datachannel.rs, crates/rv-call/src/lib.rs (`video_up_front`), captures 2026-11-23/24
  `teams.cloud.microsoft_video2.har`, `teams.cloud.microsoft_video3.har`, `teams-datachannel.json`
  (a group call's and a meeting's own camera and share)
- https://github.com/aws/amazon-chime-sdk-js/blob/main/protocol/SignalingProtocol.proto
- https://github.com/aws/amazon-chime-sdk-js/blob/main/src/signalingclient/DefaultSignalingClient.ts
- https://github.com/aws/amazon-chime-sdk-js/blob/main/src/sdp/ZLIBTextCompressor.ts
- Captures 2026-11-02: chat.barrut.me `#test` call, meet.barrut.me/config.js,
  kmeet.infomaniak.com/config.js, `app.slack.com_huddle.har`, `WebRTC Internals.html`,
  `teams.cloud.microsoft*.har` (incl. `_call_out`, `_call_in`, `_group_call`, `_group_call2`)
- https://github.com/jbg/gst-meet, https://docs.rs/crate/lib-gst-meet/latest
- https://github.com/aws/amazon-chime-sdk-cpp, https://github.com/aws/amazon-chime-sdk-js
- crates/rv-call/src/chime/video.rs, crates/rv-call/src/chime/client.rs, crates/rv-call/src/chime/mod.rs,
  crates/rv-call/src/lib.rs, apps/desktop/voice/src/call.rs (huddle video and screen share); capture 2026-11-23 `app.slack.com_huddle2.har` (both Chime sockets, camera and share)
- https://github.com/brandonp2412/microslop
