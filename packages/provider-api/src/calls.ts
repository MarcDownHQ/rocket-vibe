export type CallScope={room?:string;membership?:string|null;alive?:()=>boolean};

/** An Amazon Chime attendee's credentials, for a call the voice engine carries itself. */
export type ChimeCredentials={signalingUrl:string;audioHost:string;attendeeId:string;joinToken:string;origin:string};

/** The voice engine, for a call whose audio another part of the app carries as raw PCM (WhatsApp). */
export type PcmCallEngine={
  /** Opens the session on the audio socket at `address` (`host:port#token`). */
  connect:(address:string)=>Promise<void>;
  roster:(members:{id:string;name:string;muted:boolean}[])=>Promise<void>;
  ended:(reason:string)=>Promise<void>;
  /** Resolves when the call is hung up here. */
  left:()=>Promise<void>;
  /** Tells each change of the microphone, on or off; returns its unsubscribe. */
  microphone:(listener:(on:boolean)=>void)=>()=>void;
};

/** What the voice engine needs to join a Mattermost Calls plugin call itself. */
export type MattermostCallCredentials={server:string;token:string;channelId:string;userId:string;iceServers:{urls:string[];username:string;credential:string}[]};

/** The voice engine, for a call whose signalling the provider speaks itself. */
export type ExternalCallEngine={
  /** Opens the session under this identity. */
  start:(identity:string)=>Promise<void>;
  /** The local offer, once its candidates are gathered. */
  offer:()=>Promise<string>;
  answer:(sdp:string)=>Promise<void>;
  /** Answers an offer from the other side (a hold, a resume); null when it cannot. */
  answerOffer?:(sdp:string)=>Promise<string|null>;
  roster:(members:{id:string;name:string;muted:boolean}[])=>Promise<void>;
  /** The call ended elsewhere, for this reason. */
  ended:(reason:string)=>Promise<void>;
  /** Resolves when the call is hung up here. */
  left:()=>Promise<void>;
};

export type NativeCalls={
  available:(room?:string,membership?:string|null)=>Promise<boolean>;
  memo:()=>boolean;
  start:(room:string,membership:string|null|undefined,alive:()=>boolean)=>Promise<string>;
  join:(id:string,scope:CallScope,alive:()=>boolean,state?:{cam?:boolean;mic?:boolean})=>Promise<string>;
  decline?:(id:string)=>Promise<void>;
  /** A call the voice engine can carry over Amazon Chime: joins it and answers its credentials (Slack's huddles). */
  chime?:(id:string)=>Promise<ChimeCredentials>;
  /** A Mattermost Calls plugin call the voice engine joins itself: what it needs. */
  mattermost?:(id:string)=>Promise<MattermostCallCredentials>;
  /** Whether this room's call can run on the voice engine with `placeExternal`. */
  placeable?:(room:string)=>boolean;
  /** Places this room's call on the engine: resolves once the provider took it. */
  placeExternal?:(room:string,engine:ExternalCallEngine)=>Promise<void>;
  /** Whether the call ringing in this room can be answered on the voice engine. */
  answerable?:(room:string)=>boolean;
  /** Answers the call ringing in this room on the engine: resolves once accepted. */
  answerExternal?:(room:string,engine:ExternalCallEngine)=>Promise<void>;
  /** Carries this call (a ringing one by id, or `wa:<jid>` to call someone) with its audio on `engine`: resolves once its audio runs. */
  carryPcm?:(id:string,engine:PcmCallEngine)=>Promise<void>;
  /** The name of someone in such a call, from the id the call gives them. */
  personName?:(identity:string)=>Promise<string|null>;
};

export type CallSignal =
  | { kind: 'call'; rid: string; messageId: string; callId: string; authorId: string; ts: number }
  | { kind: 'over'; messageId: string }
  /** A call the server rang this app with itself (Teams): no message to check. */
  | { kind: 'ring'; rid: string; messageId: string; callId: string; callerId: string; callerName: string | null }
  /** Whichever call the server rang stopped ringing here (Teams: answered elsewhere or over). */
  | { kind: 'rung-over' };

/** The server's record of a call. */
export type CallCheck = { rid: string; callerId: string; callerName: string | null; over: boolean; joined: readonly string[] };
