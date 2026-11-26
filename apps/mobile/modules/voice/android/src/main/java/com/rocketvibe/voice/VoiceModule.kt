package com.rocketvibe.voice

import android.app.Activity
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.media.projection.MediaProjectionManager
import expo.modules.kotlin.Promise
import expo.modules.kotlin.functions.Queues
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition
import expo.modules.kotlin.records.Field
import expo.modules.kotlin.records.Record

class ConnectOptions : Record {
  @Field val room: String = ""
  @Field val url: String = ""
  @Field val token: String = ""
  @Field val title: String = ""
  @Field val link: String? = null
  @Field val microphone: Boolean = true
  /** An encrypted room's voice key (base64), null in a plaintext room. */
  @Field val e2eeKey: String? = null
}

/** What a Mattermost Calls plugin lets a member send besides the microphone. */
class MattermostSends : Record {
  @Field val screen: Boolean = false
  @Field val camera: Boolean = false
}

/** JS face of [VoiceEngine]: commands in, one `change` event carrying the whole snapshot out. */
class VoiceModule : Module() {
  private val listener: () -> Unit = { sendEvent("change", VoiceEngine.last) }
  private val levelListener: (Double) -> Unit = { level -> sendEvent("level", mapOf("level" to level)) }
  private val mutedSpeechListener: () -> Unit = { sendEvent("mutedSpeech", emptyMap<String, Any>()) }
  private val videoDeclinedListener: () -> Unit = { sendEvent("videoDeclined", emptyMap<String, Any>()) }
  private val hangUpListener: () -> Unit = { sendEvent("hangUp", emptyMap<String, Any>()) }
  private val offerListener: (String) -> Unit = { sdp -> sendEvent("localOffer", mapOf("sdp" to sdp)) }
  private val dataListener: (ByteArray) -> Unit = { data -> sendEvent("data", mapOf("data" to data.map { it.toInt() and 0xff })) }
  private var consent: Promise? = null

  override fun definition() = ModuleDefinition {
    Name("Voice")
    Events("change", "level", "hangUp", "localOffer", "data", "mutedSpeech", "videoDeclined")
    OnCreate {
      VoiceEngine.attach(appContext.reactContext ?: throw IllegalStateException("No React context"))
      VoiceEngine.listen(listener)
      VoiceEngine.listenLevel(levelListener)
      VoiceEngine.listenMutedSpeech(mutedSpeechListener)
      VoiceEngine.listenVideoDeclined(videoDeclinedListener)
      CallService.listen(hangUpListener)
      VoiceEngine.onLocalOffer = offerListener
      VoiceEngine.onData = dataListener
    }
    OnDestroy {
      VoiceEngine.unlisten(listener)
      VoiceEngine.unlistenLevel(levelListener)
      VoiceEngine.unlistenMutedSpeech(mutedSpeechListener)
      VoiceEngine.unlistenVideoDeclined(videoDeclinedListener)
      CallService.unlisten(hangUpListener)
      VoiceEngine.onLocalOffer = null
      VoiceEngine.onData = null
    }
    Function("snapshot") { VoiceEngine.last }
    Function("canvasRead") { load: String -> Canvas.readDocument(load) }
    Function("canvasPlan") { load: String, markdown: String, session: String, suffix: String, nowSecs: Long ->
      Canvas.planSave(load, markdown, session, suffix, nowSecs)
    }
    AsyncFunction("connect") { o: ConnectOptions ->
      VoiceEngine.connect(o.room, o.url, o.token, o.title, o.link, o.microphone, o.e2eeKey)
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("connectJitsi") { callId: String, meeting: String, displayName: String, token: String?, title: String ->
      VoiceEngine.connectJitsi(callId, meeting, displayName, token, title)
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("connectChime") { callId: String, signalingUrl: String, audioHost: String, attendeeId: String, joinToken: String, origin: String, title: String ->
      VoiceEngine.connectChime(callId, ChimeCredentials(signalingUrl, audioHost, attendeeId, joinToken, origin), title)
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("connectMattermost") { callId: String, server: String, token: String, channelId: String, userId: String, iceServers: String, title: String, sends: MattermostSends ->
      VoiceEngine.connectMattermost(callId, MattermostCall(server, token, channelId, userId, iceServers, sends.screen, sends.camera), title)
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("connectExternal") { callId: String, identity: String, title: String, answering: Boolean, video: Boolean, dataChannel: String?, receiveVideos: Int, videoUpFront: Boolean ->
      VoiceEngine.connectExternal(callId, identity, title, answering, video, dataChannel, receiveVideos, videoUpFront)
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("sendData") { data: List<Int> ->
      VoiceEngine.sendData(ByteArray(data.size) { data[it].toByte() })
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("connectPcm") { callId: String, address: String, title: String ->
      VoiceEngine.connectPcm(callId, address, title)
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("remoteAnswer") { sdp: String -> VoiceEngine.remoteAnswer(sdp) }.runOnQueue(Queues.MAIN)
    AsyncFunction("remoteOffer") { sdp: String, promise: Promise -> VoiceEngine.remoteOffer(sdp) { answer -> promise.resolve(answer) } }.runOnQueue(Queues.MAIN)
    AsyncFunction("remoteRoster") { members: List<Map<String, Any?>> ->
      VoiceEngine.remoteRoster(members.map { m ->
        Triple(m["id"] as? String ?: "", m["name"] as? String ?: "", m["muted"] as? Boolean ?: false)
      })
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("remoteVideos") { videos: List<Map<String, Any?>> ->
      VoiceEngine.remoteVideos(videos.map { v ->
        RemoteVideo(v["stream"] as? String ?: "", v["member"] as? String ?: "", if (v["screen"] == true) "screen" else "camera")
      })
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("remoteEnded") { reason: String -> VoiceEngine.remoteEnded(reason) }.runOnQueue(Queues.MAIN)
    AsyncFunction("disconnect") { VoiceEngine.leave() }.runOnQueue(Queues.MAIN)
    AsyncFunction("setE2eeKey") { key: String -> VoiceEngine.setE2eeKey(key) }.runOnQueue(Queues.MAIN)
    AsyncFunction("setMicrophone") { enabled: Boolean -> VoiceEngine.setMicrophone(enabled) }.runOnQueue(Queues.MAIN)
    AsyncFunction("setDeafened") { on: Boolean -> VoiceEngine.setDeafened(on) }.runOnQueue(Queues.MAIN)
    AsyncFunction("setRoute") { route: String -> VoiceEngine.setRoute(route) }.runOnQueue(Queues.MAIN)
    // How this side hears the call, kept by the engine for the process.
    AsyncFunction("setPersonVolume") { identity: String, volume: Double, muted: Boolean ->
      VoiceEngine.setPersonVolume(identity, volume, muted)
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("setInputVolume") { volume: Double -> VoiceEngine.setInputVolume(volume) }.runOnQueue(Queues.MAIN)
    AsyncFunction("setOutputVolume") { volume: Double -> VoiceEngine.setOutputVolume(volume) }.runOnQueue(Queues.MAIN)
    AsyncFunction("setNoiseSuppression") { on: Boolean -> VoiceEngine.setNoiseSuppression(on) }.runOnQueue(Queues.MAIN)
    AsyncFunction("setShareQuality") { height: Int, fps: Int -> VoiceEngine.setShareQuality(height, fps) }.runOnQueue(Queues.MAIN)
    AsyncFunction("ringback") { on: Boolean ->
      appContext.reactContext?.let { VoiceSounds.ringback(it, on) }
      Unit
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("ringtone") { on: Boolean ->
      appContext.reactContext?.let { VoiceSounds.ringtone(it, on) }
      Unit
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("dismissRing") { ring: String ->
      appContext.reactContext?.let { VoiceRinging.cancel(it, ring) }
      Unit
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("setCamera") { enabled: Boolean -> VoiceEngine.setCamera(enabled) }.runOnQueue(Queues.MAIN)
    // Asks Android for the screen (MediaProjection consent), then shares it. Resolves false when refused.
    AsyncFunction("startScreenShare") { promise: Promise ->
      val activity = appContext.currentActivity
      val projection = activity?.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as? MediaProjectionManager
      if (activity == null || projection == null) { promise.resolve(false); return@AsyncFunction }
      consent?.resolve(false)
      consent = promise
      activity.startActivityForResult(projection.createScreenCaptureIntent(), SCREEN_REQUEST)
    }.runOnQueue(Queues.MAIN)
    OnActivityResult { _, payload ->
      if (payload.requestCode != SCREEN_REQUEST) return@OnActivityResult
      val promise = consent ?: return@OnActivityResult
      consent = null
      val data = payload.data
      if (payload.resultCode == Activity.RESULT_OK && data != null) {
        VoiceEngine.startScreenShare(data)
        promise.resolve(true)
      } else promise.resolve(false)
    }
    AsyncFunction("stopScreenShare") { VoiceEngine.stopScreenShare() }.runOnQueue(Queues.MAIN)
    View(VoiceVideoView::class) {
      Prop("identity") { view: VoiceVideoView, identity: String -> view.identity = identity }
      Prop("source") { view: VoiceVideoView, source: String -> view.source = source }
      Prop("fit") { view: VoiceVideoView, fit: String -> view.fit = fit }
    }
    AsyncFunction("holdCall") { title: String ->
      appContext.reactContext?.let { CallService.hold(it, title) }
      Unit
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("releaseCall") {
      appContext.reactContext?.let { CallService.release(it) }
      Unit
    }.runOnQueue(Queues.MAIN)
    // Android 14 lets the user (or the store) withdraw full-screen intents: a ring is then a mere heads-up.
    Function("fullScreenRings") {
      val context = appContext.reactContext ?: return@Function true
      Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE ||
        context.getSystemService(NotificationManager::class.java)?.canUseFullScreenIntent() != false
    }
    AsyncFunction("openFullScreenRingSettings") {
      val context = appContext.reactContext ?: return@AsyncFunction Unit
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
        context.startActivity(
          Intent(Settings.ACTION_MANAGE_APP_USE_FULL_SCREEN_INTENT, Uri.parse("package:" + context.packageName))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
        )
      }
      Unit
    }.runOnQueue(Queues.MAIN)
    AsyncFunction("missed") {
      appContext.reactContext?.let { VoiceSounds.cue(it, R.raw.cue_missed) }
      Unit
    }.runOnQueue(Queues.MAIN)
  }
}

private const val SCREEN_REQUEST = 0x7276
