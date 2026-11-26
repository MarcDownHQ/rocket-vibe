import {requireNativeViewManager, requireOptionalNativeModule} from 'expo-modules-core';
import type {ComponentType} from 'react';
import type {ViewProps} from 'react-native';

export type VoiceState='idle'|'connecting'|'connected'|'reconnecting'|'disconnected';
export type VoiceRoute='speaker'|'earpiece'|'bluetooth'|'wired';
export type VoiceMember={identity:string;speaking:boolean;muted:boolean;deafened:boolean;level:number;local:boolean;camera?:boolean;screen?:boolean;
  /** A native Jitsi call's display name (the room's nick); Vibez members are named by the app. */
  name?:string};
/** The process's one voice connection, as the native engine sees it. */
export type VoiceSnapshot={
  state:VoiceState;
  /** The Vibez room this connection belongs to; null once over. */
  room?:string|null;
  microphone?:boolean;
  deafened?:boolean;
  /** The own camera is on; the own screen is shared. */
  camera?:boolean;
  sharing?:boolean;
  /** Whether this call takes the own camera, the own screen: a native call's provider may not (a Slack huddle, a Mattermost plugin without video). */
  canCamera?:boolean;
  canScreen?:boolean;
  /** Frames are end-to-end encrypted (an encrypted room). */
  encrypted?:boolean;
  /** Why the last connection ended (LiveKit's reason, lowercase): duplicate_identity, participant_removed... */
  reason?:string|null;
  route?:VoiceRoute|null;
  routes?:VoiceRoute[];
  participants:VoiceMember[];
};
export type VoiceConnect={room:string;url:string;token:string;title:string;link?:string|null;microphone:boolean;
  /** An encrypted room's voice key, base64 (docs/protocol/VOICE.md); null in a plaintext room. */
  e2eeKey?:string|null};
type Bridge={
  snapshot():VoiceSnapshot;
  connect(options:VoiceConnect):Promise<void>;
  disconnect():Promise<void>;
  /**
   * Joins a Jitsi Meet conference natively, cameras and screens included (Rocket.Chat's Jitsi
   * provider, kChat's kMeet with its `jwt`); false when this build cannot, so the meeting's page is kept.
   */
  connectJitsi(callId:string,meeting:string,displayName:string,token:string|null,title:string):Promise<boolean>;
  /** Joins an Amazon Chime meeting natively, audio only (a Slack huddle); false when this build cannot, so the huddle's page is kept. */
  connectChime(callId:string,signalingUrl:string,audioHost:string,attendeeId:string,joinToken:string,origin:string,title:string):Promise<boolean>;
  /**
   * Joins a Mattermost Calls plugin call natively; `iceServers` is the plugin's servers as JSON,
   * `sends` what the plugin lets a member send. False when this build cannot.
   */
  connectMattermost(callId:string,server:string,token:string,channelId:string,userId:string,iceServers:string,title:string,sends:{screen:boolean;camera:boolean}):Promise<boolean>;
  /**
   * A call whose signalling the app speaks itself (Teams): each offer arrives on `localOffer`,
   * the rest goes through the `remote*` functions. `answering`: the other side offers first
   * ([`remoteOffer`]), no local offer. `video`: it takes this side's camera and screen, each
   * start or stop offered anew (streams `<identity>-camera`, `<identity>-screen`). `dataChannel`:
   * a data channel the offer opens, what arrives on it coming on `data`. `receiveVideos`: receive-only
   * video sections every offer carries, the camera taking the first. `videoUpFront`: the first offer
   * carries the camera's and the screen's sections, sending nothing yet, and a start or stop makes no new offer.
   */
  connectExternal(callId:string,identity:string,title:string,answering:boolean,video:boolean,dataChannel:string|null,receiveVideos:number,videoUpFront:boolean):Promise<boolean>;
  /** A binary message on such a call's data channel, sent once it opens (bytes 0 to 255). */
  sendData(data:number[]):Promise<void>;
  /**
   * A call the app's WhatsApp engine carries (its socket at `address`, `host:port#token`): its audio,
   * and its camera or screen both ways when the engine takes a video connection. False when this build cannot.
   */
  connectPcm(callId:string,address:string,title:string):Promise<boolean>;
  remoteAnswer(sdp:string):Promise<void>;
  /** Answers an offer from the other side of such a call (its camera or screen, a hold): the local answer, null when it cannot. */
  remoteOffer(sdp:string):Promise<string|null>;
  remoteRoster(members:{id:string;name:string;muted:boolean}[]):Promise<void>;
  /** Every video the other side of such a call sends now, by the stream id its section carries: whose, and whether a shared screen. */
  remoteVideos(videos:{stream:string;member:string;screen:boolean}[]):Promise<void>;
  remoteEnded(reason:string):Promise<void>;
  /** A Slack canvas's `load-data` answer (base64) as JSON `{markdown, version, thread, document, title}`; null when unreadable. */
  canvasRead(load:string):string|null;
  /** The `data_binary` (base64) saving `markdown` over that answer, as JSON `{data, changed}`. */
  canvasPlan(load:string,markdown:string,session:string,suffix:string,nowSecs:number):string|null;
  /** Replaces the frame key when the room's group reaches a new epoch. */
  setE2eeKey(key:string):Promise<void>;
  setMicrophone(enabled:boolean):Promise<void>;
  setDeafened(on:boolean):Promise<void>;
  setRoute(route:VoiceRoute):Promise<void>;
  setCamera(enabled:boolean):Promise<void>;
  /** Asks Android for the screen, then shares it; false when the user refused. */
  startScreenShare():Promise<boolean>;
  stopScreenShare():Promise<void>;
  ringback(on:boolean):Promise<void>;
  ringtone(on:boolean):Promise<void>;
  missed():Promise<void>;
  /** Stops the system ring of an incoming call the app now shows itself. */
  dismissRing(ring:string):Promise<void>;
  /** How loud someone plays here (0 to 2) and a mute for this side only; kept for the process. */
  setPersonVolume(identity:string,volume:number,muted:boolean):Promise<void>;
  setInputVolume(volume:number):Promise<void>;
  setOutputVolume(volume:number):Promise<void>;
  /** The noise remover (RNNoise) on the microphone. */
  setNoiseSuppression(on:boolean):Promise<void>;
  /** The next screen share's lines (the screen's shorter side) and frames a second. */
  setShareQuality(height:number,fps:number):Promise<void>;
  /** Keeps a call a web page holds (Jitsi, kMeet) alive in the background, behind an ongoing call notification. */
  holdCall(title:string):Promise<void>;
  releaseCall():Promise<void>;
  /** Android 14 and later can withdraw the full-screen intent: a ring is then only a notification. */
  fullScreenRings():boolean;
  openFullScreenRingSettings():Promise<void>;
  addListener(event:'change',listener:(snapshot:VoiceSnapshot)=>void):{remove:()=>void};
  /** The held call's notification hung up. */
  addListener(event:'hangUp',listener:()=>void):{remove:()=>void};
  addListener(event:'localOffer',listener:(event:{sdp:string})=>void):{remove:()=>void};
  /** A binary message on an external call's data channel (bytes 0 to 255). */
  addListener(event:'data',listener:(event:{data:number[]})=>void):{remove:()=>void};
  /** Someone spoke with the microphone off in a voice room (local only, nothing sent). */
  addListener(event:'mutedSpeech',listener:()=>void):{remove:()=>void};
  /** The microphone's level, 0 to 1, about ten times a second while connected. */
  addListener(event:'level',listener:(event:{level:number})=>void):{remove:()=>void};
  /** The other side of a WhatsApp call refused, or let lapse, the switch to video this side asked for: camera and screen are off. */
  addListener(event:'videoDeclined',listener:()=>void):{remove:()=>void};
};
/**
 * LiveKit audio in a microphone foreground service (Android). Absent from a
 * build without the module: the app then never offers voice.
 */
export const VoiceNative=requireOptionalNativeModule<Bridge>('Voice');

export type VoiceVideoProps=ViewProps&{identity:string;source:'camera'|'screen';fit?:'cover'|'contain'};
/** A participant's camera or screen, from the engine's room or native call. Null without the module. */
export const VoiceVideoView:ComponentType<VoiceVideoProps>|null=VoiceNative?requireNativeViewManager<VoiceVideoProps>('Voice'):null;
