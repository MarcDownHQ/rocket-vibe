package com.rocketvibe.voice

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import livekit.org.webrtc.AudioTrack
import livekit.org.webrtc.DataChannel
import livekit.org.webrtc.DefaultVideoDecoderFactory
import livekit.org.webrtc.DefaultVideoEncoderFactory
import livekit.org.webrtc.IceCandidate
import livekit.org.webrtc.MediaConstraints
import livekit.org.webrtc.MediaStream
import livekit.org.webrtc.MediaStreamTrack
import livekit.org.webrtc.PeerConnection
import livekit.org.webrtc.PeerConnectionFactory
import livekit.org.webrtc.RtpReceiver
import livekit.org.webrtc.RtpSender
import livekit.org.webrtc.RtpTransceiver
import livekit.org.webrtc.SdpObserver
import livekit.org.webrtc.SessionDescription
import livekit.org.webrtc.VideoSource
import livekit.org.webrtc.VideoTrack
import livekit.org.webrtc.audio.JavaAudioDeviceModule
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap

/**
 * A Jitsi Meet conference's signalling (crates/rv-voice-mobile's call module,
 * rv-call as the desktop sidecar speaks it) over JNI. Absent library: [available]
 * is false and calls keep the meeting's page.
 */
internal object JitsiCall {
  val available: Boolean = try {
    System.loadLibrary("rv_voice_mobile")
    true
  } catch (error: UnsatisfiedLinkError) {
    false
  }

  @JvmStatic external fun start(meeting: String, configJs: String, displayName: String, token: String?): Long
  @JvmStatic external fun startMattermost(server: String, token: String, channelId: String, userId: String, iceServers: String, screen: Boolean, camera: Boolean): Long
  @JvmStatic external fun startChime(signalingUrl: String, audioHost: String, attendeeId: String, joinToken: String, origin: String): Long
  @JvmStatic external fun offer(handle: Long, sdp: String)
  @JvmStatic external fun next(handle: Long, timeoutMs: Long): String?
  @JvmStatic external fun answer(handle: Long, sdp: String)
  @JvmStatic external fun candidate(handle: Long, mid: String, line: String)
  @JvmStatic external fun muted(handle: Long, muted: Boolean)
  /** This side's camera or screen went on or off its peer connection: the provider renegotiates. */
  @JvmStatic external fun video(handle: Long, camera: Boolean, screen: Boolean)
  /** A remote video track arrived: a `named` event follows when its own ids say whose it is. */
  @JvmStatic external fun remoteVideo(handle: Long, track: String, stream: String)
  /** This side's screen capture is ready, a session of its own then joining, or stopped, that session left. */
  @JvmStatic external fun share(handle: Long, on: Boolean)
  /** That session's offer, its candidates in it. */
  @JvmStatic external fun shareOffer(handle: Long, sdp: String)
  @JvmStatic external fun leave(handle: Long)
  @JvmStatic external fun free(handle: Long)
}

/**
 * What joining a Mattermost Calls plugin call takes; `iceServers` is the plugin's
 * servers as JSON, `screen` and `camera` what the plugin lets a member send.
 */
internal data class MattermostCall(
  val server: String,
  val token: String,
  val channelId: String,
  val userId: String,
  val iceServers: String,
  val screen: Boolean,
  val camera: Boolean,
)

/** An Amazon Chime attendee's credentials (a Slack huddle). */
internal data class ChimeCredentials(
  val signalingUrl: String,
  val audioHost: String,
  val attendeeId: String,
  val joinToken: String,
  val origin: String,
)

/**
 * One native call: libwebrtc (the copy LiveKit ships) answers a Jitsi bridge's
 * offer or offers to Chime and Mattermost, the signalling runs in Rust. Besides
 * the microphone it sends this side's camera and screen where the provider takes
 * them, and receives everyone's. [onChange] and [onEnd] are called on the main thread.
 */
internal class JitsiSession(
  private val app: Context,
  private val onChange: () -> Unit,
  private val onEnd: (String) -> Unit,
) {
  private val main = Handler(Looper.getMainLooper())
  @Volatile private var handle = 0L
  @Volatile private var closed = false
  private var factory: PeerConnectionFactory? = null
  private var audio: JavaAudioDeviceModule? = null
  private var pc: PeerConnection? = null
  private var microphone: AudioTrack? = null
  private var iceServers: List<PeerConnection.IceServer> = emptyList()
  private var localId = ""
  /** The data channel the provider wants opened by the offer (Mattermost's SFU), by its label. */
  private var channelLabel: String? = null
  private var channelProtocol = ""
  private var channel: DataChannel? = null
  /** The provider's messages for that channel, until it opens, each with whether it is binary. */
  private val outbox = ArrayList<Pair<ByteArray, Boolean>>()
  /** What arrives on that channel, for an app that speaks it (Teams). */
  var onData: ((ByteArray) -> Unit)? = null
  /** The receive-only video sections each offer carries, never fewer, and those the current peer connection has. */
  private var receivers = 0
  private var addedReceivers = 0
  /** The provider's offers receive video (a huddle, once someone's video was wanted). */
  private var receiveVideo = false
  /** What the provider takes from this side, and the stream id each local video track carries. */
  var canCamera = false; private set
  var canScreen = false; private set
  private val streams = HashMap<String, String>()
  /** The kinds whose local track goes on the first unused section, the provider's offers carrying this side's (Jitsi), else on a section of its own. */
  private var reused = emptySet<String>()
  /** The screen goes on a session of its own (Chime's `#content` attendee), whose peer connection sends it alone; [shared] once that one connected. */
  private var screenApart = false
  private var share: Share? = null
  private var shared = false
  /** Called when the screen's own session failed before it sent anything: the capture is already off. */
  var onScreenUnavailable: (() -> Unit)? = null
  /** This side's captures, kept across media sessions; their tracks and senders, for the current peer connection. */
  private val captures = HashMap<String, LocalCapture>()
  private val localTracks = HashMap<String, Pair<VideoSource, VideoTrack>>()
  private val senders = HashMap<String, RtpSender>()
  /** What the provider was last told this side sends: camera, screen. */
  private var told = false to false
  /** Answers refused in a row, each answered by offering again. */
  private var reoffers = 0
  /** An offer of this side's being made, and one wanted while the last one waited for its answer. */
  private var making = false
  private var deferred = false
  /** Remote video tracks by id, with their stream; the provider's list of them, and those named by their own ids. */
  private val remoteVideos = HashMap<String, Pair<String, VideoTrack>>()
  private var listed = false
  private var videos: List<RemoteVideo> = emptyList()
  private val named = HashMap<String, RemoteVideo>()
  private var deafened = false
  /** Everyone else: endpoint id to display name and whether they are muted. */
  private val roster = LinkedHashMap<String, Pair<String, Boolean>>()
  /** Remote tracks by endpoint, and when each was last heard. */
  private val tracks = ConcurrentHashMap<String, AudioTrack>()
  private val heard = ConcurrentHashMap<String, Long>()
  private val levels = ConcurrentHashMap<String, Double>()
  /** The stream of the last remote track: one person's voice when only one is here. */
  @Volatile private var mixed: String? = null
  @Volatile var connected = false; private set
  /** Lost on the way and joining again. */
  @Volatile var reconnecting = false; private set
  /** Why the current session was lost (a media failure), read by the reader thread. */
  @Volatile private var lost: String? = null
  private var everConnected = false

  /** The offer waiting for ICE gathering, sent once at most. */
  private var offering = false
  /** The app signals this call: the offer goes to [onLocalOffer], and a loss ends it. */
  private var external = false
  /** That call takes this side's camera and screen, each start or stop offered anew. */
  private var externalVideo = false
  /** Their sections come in the first offer, sending nothing yet: a start or stop only swaps the track. */
  private var videoUpFront = false
  var onLocalOffer: ((String) -> Unit)? = null
  private var relayOnly = false

  /** A call carried as raw PCM by the app's WhatsApp engine: its socket, while it runs. */
  @Volatile private var pcm: java.net.Socket? = null
  /** That call's video connection, when the engine takes one. */
  @Volatile private var pcmVideo: PcmVideo? = null
  /** The peer refused the video this side asked for: the captures are already off. */
  var onVideoRefused: (() -> Unit)? = null

  /**
   * A call whose audio another part of the app carries (a WhatsApp call, the
   * engine's socket at `address`, `host:port#token`): 16 kHz mono in 60 ms
   * frames of 960 little-endian samples both ways, recorded and played here
   * with the phone's echo canceller. Who is in it comes with [remoteRoster];
   * the socket closing ends it. A second connection carries the camera or
   * screen both ways ([PcmVideo]).
   */
  fun startPcm(address: String): Boolean {
    val hostPort = address.substringBefore('#')
    val token = address.substringAfter('#', "")
    val host = hostPort.substringBeforeLast(':')
    val port = hostPort.substringAfterLast(':').toIntOrNull() ?: return false
    external = true
    localId = "me"
    Thread({ runPcm(host, port, token) }, "pcm-call").start()
    return true
  }

  @android.annotation.SuppressLint("MissingPermission")
  private fun runPcm(host: String, port: Int, token: String) {
    val rate = 16_000
    val samples = 960
    val socket = runCatching {
      java.net.Socket().apply { tcpNoDelay = true; connect(java.net.InetSocketAddress(host, port), 5_000) }
    }.getOrNull() ?: return main.post { if (!closed) onEnd("connect_failed") }.let {}
    pcm = socket
    val output = socket.getOutputStream()
    val input = java.io.DataInputStream(socket.getInputStream())
    runCatching { output.write("$token\n".toByteArray()); output.flush() }
    openPcmVideo(host, port, token)
    val inBuffer = maxOf(android.media.AudioRecord.getMinBufferSize(rate, android.media.AudioFormat.CHANNEL_IN_MONO, android.media.AudioFormat.ENCODING_PCM_16BIT), samples * 2 * 2)
    val record = runCatching {
      android.media.AudioRecord(android.media.MediaRecorder.AudioSource.VOICE_COMMUNICATION, rate,
        android.media.AudioFormat.CHANNEL_IN_MONO, android.media.AudioFormat.ENCODING_PCM_16BIT, inBuffer)
    }.getOrNull()
    if (record == null || record.state != android.media.AudioRecord.STATE_INITIALIZED) {
      runCatching { socket.close() }
      return main.post { if (!closed) onEnd("connect_failed") }.let {}
    }
    val echo = if (android.media.audiofx.AcousticEchoCanceler.isAvailable()) android.media.audiofx.AcousticEchoCanceler.create(record.audioSessionId) else null
    echo?.enabled = true
    val noise = if (android.media.audiofx.NoiseSuppressor.isAvailable()) android.media.audiofx.NoiseSuppressor.create(record.audioSessionId) else null
    noise?.enabled = true
    val player = android.media.AudioTrack.Builder()
      .setAudioAttributes(android.media.AudioAttributes.Builder()
        .setUsage(android.media.AudioAttributes.USAGE_VOICE_COMMUNICATION)
        .setContentType(android.media.AudioAttributes.CONTENT_TYPE_SPEECH).build())
      .setAudioFormat(android.media.AudioFormat.Builder().setSampleRate(rate)
        .setEncoding(android.media.AudioFormat.ENCODING_PCM_16BIT)
        .setChannelMask(android.media.AudioFormat.CHANNEL_OUT_MONO).build())
      .setBufferSizeInBytes(samples * 2 * 4)
      .setTransferMode(android.media.AudioTrack.MODE_STREAM)
      .build()
    record.startRecording()
    player.play()
    main.post { if (!closed) { connected = true; everConnected = true; onChange() } }
    val sender = Thread({
      val frame = ShortArray(samples)
      val bytes = ByteArray(samples * 2)
      while (!closed) {
        var read = 0
        while (read < samples && !closed) {
          val n = record.read(frame, read, samples - read)
          if (n <= 0) break
          read += n
        }
        if (read < samples) break
        val silent = !VoiceEngine.microphone || VoiceEngine.deafened
        var peak = 0
        for (i in 0 until samples) {
          val s = if (silent) 0 else frame[i].toInt()
          if (kotlin.math.abs(s) > peak) peak = kotlin.math.abs(s)
          bytes[2 * i] = (s and 0xff).toByte(); bytes[2 * i + 1] = ((s shr 8) and 0xff).toByte()
        }
        levels[localId] = peak / 32_768.0
        if (runCatching { output.write(bytes) }.isFailure) break
      }
    }, "pcm-call-out")
    sender.start()
    val bytes = ByteArray(samples * 2)
    val frame = ShortArray(samples)
    while (!closed && runCatching { input.readFully(bytes) }.isSuccess) {
      var peak = 0
      for (i in 0 until samples) {
        val s = ((bytes[2 * i + 1].toInt() shl 8) or (bytes[2 * i].toInt() and 0xff)).toShort()
        frame[i] = if (deafened) 0 else s
        if (kotlin.math.abs(s.toInt()) > peak) peak = kotlin.math.abs(s.toInt())
      }
      val level = peak / 32_768.0
      roster.keys.firstOrNull()?.let { peer ->
        levels[peer] = level
        if (level > SPEAKING) heard[peer] = SystemClock.elapsedRealtime()
      }
      player.write(frame, 0, samples)
    }
    runCatching { socket.close() }
    pcmVideo?.close()
    sender.join(1_000)
    record.stop(); record.release(); echo?.release(); noise?.release()
    player.stop(); player.release()
    pcm = null
    main.post { if (!closed) onEnd("remote_ended") }
  }

  private fun openPcmVideo(host: String, port: Int, token: String) {
    // The encoder's and the views' buffers live in WebRTC's native library.
    runCatching { PeerConnectionFactory.initialize(PeerConnectionFactory.InitializationOptions.builder(app).createInitializationOptions()) }
      .onFailure { return }
    val video = PcmVideo.open(host, port, token, onChange = { main.post { if (!closed) onChange() } }, onRefused = {
      main.post {
        if (closed || (captures[CAMERA] == null && captures[SCREEN] == null)) return@post
        captures.remove(CAMERA)?.stop()
        captures.remove(SCREEN)?.stop()
        syncVideo()
        onVideoRefused?.invoke()
      }
    }) ?: return
    main.post {
      if (closed) return@post video.close()
      pcmVideo = video
      canCamera = true
      canScreen = true
      onChange()
    }
  }

  /** Joins on a thread of its own; false at once when the library is missing. */
  fun start(meeting: String, displayName: String, token: String?): Boolean {
    if (!JitsiCall.available) return false
    Thread({
      val configJs = fetchConfig(meeting)
      hold { if (configJs == null) 0L else JitsiCall.start(meeting, configJs, displayName, token) }
    }, "jitsi-call").start()
    return true
  }

  /**
   * A call whose signalling the app speaks itself: this side offers through
   * [onLocalOffer] and takes the answer with [remoteAnswer], or answers the
   * other side's first offer ([remoteOffer]) when `answering`. With `video` it
   * sends the camera, on the first unused video section, and the screen, on a
   * section of its own, their streams `<identity>-camera` and `<identity>-screen`,
   * and offers anew after each start or stop once the last offer was answered.
   * `dataChannel` names a data channel the offer opens, its messages going to
   * [onData]; `receiveVideos` receive-only video sections go in every offer, the
   * camera taking the first. With `videoUpFront` the first offer carries a camera
   * and a screen section, both sending and receiving with no track yet, and a start
   * or stop swaps the track with no new offer. Needs no library: nothing here crosses JNI. Called on the main thread.
   */
  fun startExternal(identity: String, answering: Boolean, video: Boolean, dataChannel: String? = null, receiveVideos: Int = 0, videoUpFront: Boolean = false) {
    external = true
    externalVideo = video
    this.videoUpFront = video && videoUpFront
    channelLabel = dataChannel?.takeIf { it.isNotEmpty() }
    channelProtocol = ""
    receivers = receiveVideos.coerceAtLeast(0)
    localId = identity
    canCamera = video
    canScreen = video
    streams[CAMERA] = "$identity-$CAMERA"
    streams[SCREEN] = "$identity-$SCREEN"
    reused = setOf(CAMERA)
    if (answering) return
    // After the engine set this session's microphone state, which the track reads.
    main.post { if (!closed) localOffer() }
  }

  /** The other side's answer to this side's offer; one that cannot be set ends a call not yet connected, and leaves a running one as it was. */
  fun remoteAnswer(sdp: String) {
    val p = pc ?: return
    p.setRemoteDescription(object : Sdp() {
      override fun onSetSuccess() { main.post { if (!closed && pc === p) { syncVideo(); offerDeferred() } } }
      override fun onSetFailure(error: String?) {
        super.onSetFailure(error)
        main.post {
          if (closed || pc !== p) return@post
          if (!connected) onEnd("connect_failed") else offerDeferred()
        }
      }
    }, SessionDescription(SessionDescription.Type.ANSWER, sdp))
  }

  /**
   * Answers an offer from the other side of an external call (its camera, its
   * screen, a hold); null when it cannot. An offer of this side's waiting for its
   * answer is rolled back first, and made again once this one is answered.
   */
  fun remoteOffer(sdp: String, done: (String?) -> Unit) {
    val p = pc ?: peer() ?: return done(null)
    val yielded = p.signalingState() == PeerConnection.SignalingState.HAVE_LOCAL_OFFER
    val failed = object : Sdp() {
      override fun onCreateFailure(error: String?) { super.onCreateFailure(error); main.post { done(null) } }
      override fun onSetFailure(error: String?) { super.onSetFailure(error); main.post { done(null) } }
    }
    val answer = {
      p.setRemoteDescription(object : Sdp() {
        override fun onSetSuccess() {
          p.createAnswer(object : Sdp() {
            override fun onCreateSuccess(answer: SessionDescription) {
              p.setLocalDescription(object : Sdp() {
                override fun onSetSuccess() {
                  main.post {
                    done(p.localDescription?.description ?: answer.description)
                    if (closed || pc !== p) return@post
                    if (yielded) deferred = true
                    syncVideo()
                    offerDeferred()
                  }
                }
                override fun onSetFailure(error: String?) { failed.onSetFailure(error) }
              }, answer)
            }
            override fun onCreateFailure(error: String?) { failed.onCreateFailure(error) }
          }, MediaConstraints())
        }
        override fun onSetFailure(error: String?) { failed.onSetFailure(error) }
      }, SessionDescription(SessionDescription.Type.OFFER, sdp))
    }
    if (!yielded) return answer()
    offering = false
    p.setLocalDescription(object : Sdp() {
      override fun onSetSuccess() { main.post { if (pc === p) answer() else done(null) } }
      override fun onSetFailure(error: String?) { super.onSetFailure(error); main.post { if (pc === p) answer() else done(null) } }
    }, SessionDescription(SessionDescription.Type.ROLLBACK, ""))
  }

  /** Names the other side's videos of an external call by their stream: whose, and what. */
  fun listRemoteVideos(list: List<RemoteVideo>) {
    videos = list
    listed = true
    onChange()
  }

  fun remoteRoster(members: List<Triple<String, String, Boolean>>) {
    roster.clear()
    members.forEach { (id, name, muted) -> roster[id] = name to muted }
    onChange()
  }

  /** Joins a Mattermost call on a thread of its own; false at once when the library is missing. */
  fun startMattermost(c: MattermostCall): Boolean {
    if (!JitsiCall.available) return false
    Thread({
      hold { JitsiCall.startMattermost(c.server, c.token, c.channelId, c.userId, c.iceServers, c.screen, c.camera) }
    }, "mattermost-call").start()
    return true
  }

  /** Joins a Chime meeting on a thread of its own; false at once when the library is missing. */
  fun startChime(c: ChimeCredentials): Boolean {
    if (!JitsiCall.available) return false
    Thread({
      hold { JitsiCall.startChime(c.signalingUrl, c.audioHost, c.attendeeId, c.joinToken, c.origin) }
    }, "chime-call").start()
    return true
  }

  /**
   * Joins, then joins again whenever the session is lost on the way (the socket
   * dropped, the media failed, the focus ended or replaced the session), keeping
   * the microphone and deafen choices, until it holds or every retry failed.
   * A first join that fails ends at once, so the meeting's page takes over.
   */
  private fun hold(join: () -> Long) {
    var tries = 0
    var reason = "connect_failed"
    var over = false
    while (!closed) {
      val h = join()
      if (h == 0L && !everConnected) break
      if (h != 0L) {
        handle = h
        lost = null
        while (!closed && lost == null) {
          val line = JitsiCall.next(h, 500) ?: continue
          val event = runCatching { JSONObject(line) }.getOrNull() ?: continue
          val type = event.optString("type")
          if (type in setOf("ended", "gone", "over")) {
            over = type == "over"
            lost = event.optString("reason", "terminated")
            break
          }
          main.post { if (!closed) handle(event) }
        }
        // Only this thread enters the library once joined: leaving and freeing never race a read.
        handle = 0L
        JitsiCall.leave(h)
        JitsiCall.free(h)
        if (closed) return
        reason = lost ?: "terminated"
        if (connected) tries = 0
      }
      if (over || tries >= REJOIN_DELAYS_S.size) break
      Log.i("RocketVibeVoice", "jitsi: lost ($reason), joining again")
      reconnecting = true
      connected = false
      main.post { if (!closed) { dropMedia(); onChange() } }
      Thread.sleep(REJOIN_DELAYS_S[tries++] * 1000)
    }
    if (!closed) main.post { if (!closed) onEnd(reason) }
  }

  private fun fetchConfig(meeting: String): String? = runCatching {
    val url = URL(meeting)
    if (url.protocol != "https") return null
    val connection = URL("https://${url.host}/config.js").openConnection() as HttpURLConnection
    connection.connectTimeout = 5000; connection.readTimeout = 5000
    try { connection.inputStream.bufferedReader().readText() } finally { connection.disconnect() }
  }.getOrNull()

  private fun handle(event: JSONObject) {
    when (event.optString("type")) {
      "local" -> {
        localId = event.optString("id")
        channelLabel = event.optString("dataChannel").takeIf { it.isNotEmpty() && !event.isNull("dataChannel") }
        channelProtocol = event.optString("dataProtocol")
        canCamera = event.optBoolean("canCamera")
        canScreen = event.optBoolean("canScreen")
        streams[CAMERA] = event.optString("cameraStream", localId)
        streams[SCREEN] = event.optString("screenStream", localId)
        reused = if (event.optBoolean("offersLocalSections")) setOf(CAMERA, SCREEN) else emptySet()
        screenApart = event.optBoolean("screenApart")
        receivers = 0
        receiveVideo = false
        // A capture the provider does not take ends (a rejoin keeps what it still takes).
        if (!canCamera) captures.remove(CAMERA)?.stop()
        if (!canScreen) captures.remove(SCREEN)?.let { it.stop(); onScreenStopped?.invoke() }
        onChange()
      }
      "videos" -> {
        val list = event.optJSONArray("videos")
        videos = (0 until (list?.length() ?: 0)).map { i ->
          val v = list!!.getJSONObject(i)
          RemoteVideo(v.getString("track"), v.getString("member"), v.optString("kind", CAMERA))
        }
        listed = true
        onChange()
      }
      "named" -> {
        val track = event.getString("track")
        named[track] = RemoteVideo(track, event.getString("member"), event.optString("kind", CAMERA))
        onChange()
      }
      "data" -> {
        outbox.add(event.getString("text").toByteArray() to false)
        flush()
      }
      "ice" -> iceServers = iceServersOf(event)
      "offer" -> offer(event.getString("sdp"))
      "offerWanted" -> {
        relayOnly = event.optBoolean("relayOnly")
        receivers = maxOf(receivers, event.optInt("receiveVideos"))
        receiveVideo = receiveVideo || event.optBoolean("receiveVideo")
        localOffer()
      }
      "answer" -> remoteAnswerOfProvider(event.getString("sdp"))
      "share" -> handleShare(event.getJSONObject("event"))
      "levels" -> {
        val list = event.getJSONArray("levels")
        val now = SystemClock.elapsedRealtime()
        for (i in 0 until list.length()) {
          val item = list.getJSONObject(i)
          val id = item.getString("id")
          val level = item.optDouble("level")
          levels[id] = level
          if (level > REPORTED_SPEAKING) heard[id] = now
        }
      }
      // The focus ended the media of a participant left alone: still in the call, the next offer reopens it.
      "media_ended" -> {
        val members = LinkedHashMap(roster)
        dropMedia()
        roster.putAll(members)
        onChange()
      }
      "candidate" -> pc?.addIceCandidate(IceCandidate(event.getString("mid"), 0, event.getString("line")))
      "roster" -> {
        roster.clear()
        val members = event.getJSONArray("members")
        for (i in 0 until members.length()) {
          val m = members.getJSONObject(i)
          roster[m.getString("id")] = m.optString("name") to m.optBoolean("muted")
        }
        onChange()
      }
    }
  }

  private fun peer(): PeerConnection? {
    pc?.let { return it }
    PeerConnectionFactory.initialize(PeerConnectionFactory.InitializationOptions.builder(app).createInitializationOptions())
    val adm = JavaAudioDeviceModule.builder(app)
      .setUseHardwareAcousticEchoCanceler(true)
      .setUseHardwareNoiseSuppressor(true)
      .createAudioDeviceModule()
    val egl = NativeVideo.egl.eglBaseContext
    val f = PeerConnectionFactory.builder().setAudioDeviceModule(adm)
      .setVideoEncoderFactory(DefaultVideoEncoderFactory(egl, true, true))
      .setVideoDecoderFactory(DefaultVideoDecoderFactory(egl))
      .createPeerConnectionFactory()
    audio = adm; factory = f
    val created = f.createPeerConnection(rtcConfiguration(iceServers, relayOnly), observer) ?: return null
    val track = f.createAudioTrack("${localId}-a0", f.createAudioSource(MediaConstraints()))
    track.setEnabled(VoiceEngine.microphone && !VoiceEngine.deafened)
    created.addTrack(track, listOf(localId))
    channelLabel?.let { label ->
      channel = created.createDataChannel(label, DataChannel.Init().apply { protocol = channelProtocol })?.also { dc ->
        dc.registerObserver(object : DataChannel.Observer {
          override fun onBufferedAmountChange(previous: Long) {}
          override fun onStateChange() { main.post { if (!closed) flush() } }
          override fun onMessage(buffer: DataChannel.Buffer) {
            if (!external) return
            val data = ByteArray(buffer.data.remaining()).also { buffer.data.get(it) }
            main.post { if (!closed) onData?.invoke(data) }
          }
        })
      }
    }
    if (videoUpFront) prepareVideo(created)
    addReceivers(created)
    microphone = track
    pc = created
    attachVideo()
    return created
  }

  /** A sender for the camera and one for the screen, with no track yet, on sections that both send and receive. */
  private fun prepareVideo(p: PeerConnection) {
    for (kind in listOf(CAMERA, SCREEN)) {
      if (senders.containsKey(kind)) continue
      val init = RtpTransceiver.RtpTransceiverInit(RtpTransceiver.RtpTransceiverDirection.SEND_RECV, listOf(streams[kind] ?: localId))
      runCatching { p.addTransceiver(MediaStreamTrack.MediaType.MEDIA_TYPE_VIDEO, init) }.getOrNull()?.let { senders[kind] = it.sender }
        ?: Log.w("RocketVibeVoice", "jitsi: $kind sender refused")
    }
  }

  /** The servers of an `ice` event. */
  private fun iceServersOf(event: JSONObject): List<PeerConnection.IceServer> {
    val servers = event.optJSONArray("servers")
    return (0 until (servers?.length() ?: 0)).map { i ->
      val s = servers!!.getJSONObject(i)
      val urls = s.getJSONArray("urls").let { a -> (0 until a.length()).map { a.getString(it) } }
      PeerConnection.IceServer.builder(urls).setUsername(s.optString("username")).setPassword(s.optString("credential")).createIceServer()
    }
  }

  private fun rtcConfiguration(servers: List<PeerConnection.IceServer>, relay: Boolean) = PeerConnection.RTCConfiguration(servers).apply {
    sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
    bundlePolicy = PeerConnection.BundlePolicy.MAXBUNDLE
    rtcpMuxPolicy = PeerConnection.RtcpMuxPolicy.REQUIRE
    if (relay) iceTransportsType = PeerConnection.IceTransportsType.RELAY
  }

  /** Receive-only video sections up to [receivers], none ever removed. */
  private fun addReceivers(p: PeerConnection) {
    while (addedReceivers < receivers) {
      p.addTransceiver(MediaStreamTrack.MediaType.MEDIA_TYPE_VIDEO,
        RtpTransceiver.RtpTransceiverInit(RtpTransceiver.RtpTransceiverDirection.RECV_ONLY))
      addedReceivers++
    }
  }

  /** The provider's messages, once its data channel is open. */
  private fun flush() {
    val dc = channel ?: return
    if (dc.state() != DataChannel.State.OPEN) return
    for ((data, binary) in outbox) dc.send(DataChannel.Buffer(ByteBuffer.wrap(data), binary))
    outbox.clear()
  }

  /** A binary message on the provider's data channel, sent once it is open. */
  fun sendData(data: ByteArray) {
    outbox.add(data to true)
    flush()
  }

  /**
   * Each capture on its sender of the current peer connection, each stopped one
   * off it: on the first unused section for a kind [reused] names, else on a
   * section of its own, never one opened for another's track.
   */
  private fun attachVideo() {
    val p = pc ?: return
    val f = factory ?: return
    for (kind in if (screenApart) listOf(CAMERA) else listOf(CAMERA, SCREEN)) {
      val capture = captures[kind]
      val current = localTracks[kind]
      if (capture != null && current == null) {
        val source = f.createVideoSource(kind == SCREEN)
        val track = f.createVideoTrack("$localId-$kind", source)
        val sender = senders[kind]
        val attached = if (sender != null) sender.setTrack(track, false) else {
          val stream = streams[kind] ?: localId
          runCatching {
            if (kind in reused) p.addTrack(track, listOf(stream))
            else p.addTransceiver(track, RtpTransceiver.RtpTransceiverInit(RtpTransceiver.RtpTransceiverDirection.SEND_ONLY, listOf(stream))).sender
          }.getOrNull()?.also { senders[kind] = it } != null
        }
        if (attached) {
          capture.target = source.capturerObserver
          localTracks[kind] = source to track
        } else {
          Log.w("RocketVibeVoice", "jitsi: $kind track refused")
          track.dispose(); source.dispose()
          captures.remove(kind)?.stop()
          if (kind == SCREEN) onScreenStopped?.invoke()
        }
      } else if (capture == null && current != null) {
        senders[kind]?.setTrack(null, false)
        localTracks.remove(kind)
        current.second.dispose(); current.first.dispose()
      }
    }
  }

  /** This side's video on the peer connection, or the screen on a session of its own, then the provider told what is sent. */
  private fun syncVideo() {
    pcmVideo?.let { video ->
      video.send(captures[SCREEN] ?: captures[CAMERA], captures[SCREEN] != null)
      return onChange()
    }
    if (pc == null) return onChange()
    if (screenApart) {
      val screen = captures[SCREEN]
      val current = share
      if (screen == null) closeShare()
      else if (current == null && handle != 0L) {
        share = Share()
        JitsiCall.share(handle, true)
      } else current?.source?.let { if (screen.target == null) screen.target = it.capturerObserver }
    }
    attachVideo()
    val now = (localTracks[CAMERA] != null) to (if (screenApart) shared else localTracks[SCREEN] != null)
    if (now != told) {
      told = now
      if (handle != 0L) JitsiCall.video(handle, now.first, now.second)
      else if (external && externalVideo && !videoUpFront) offerInTurn()
    }
    onChange()
  }

  /** An external call's new offer, now if the last one was answered, else once it is. */
  private fun offerInTurn() {
    val p = pc ?: return
    if (making || p.signalingState() != PeerConnection.SignalingState.STABLE) { deferred = true; return }
    localOffer()
  }

  /** The offer wanted while the last one waited, once the connection is stable again. */
  private fun offerDeferred() {
    if (!deferred || making || pc?.signalingState() != PeerConnection.SignalingState.STABLE) return
    deferred = false
    localOffer()
  }

  /**
   * This side's screen on a session of its own and a peer connection that sends
   * it alone, with a microphone that sends silence, as Chime's web client does.
   */
  private inner class Share {
    var id = ""
    var ice: List<PeerConnection.IceServer> = emptyList()
    var relayOnly = false
    var pc: PeerConnection? = null
    var source: VideoSource? = null
    var track: VideoTrack? = null
    var offering = false
    var connected = false

    fun close() {
      val screen = source?.capturerObserver
      captures[SCREEN]?.let { if (screen != null && it.target === screen) it.target = null }
      pc?.close(); pc?.dispose(); pc = null
      track?.dispose(); source?.dispose()
      track = null; source = null
    }
  }

  private fun handleShare(event: JSONObject) {
    val s = share ?: return
    when (event.optString("type")) {
      "local" -> s.id = event.optString("id")
      "ice" -> s.ice = iceServersOf(event)
      "offerWanted" -> {
        s.relayOnly = event.optBoolean("relayOnly")
        offerShare(s)
      }
      "answer" -> s.pc?.setRemoteDescription(object : Sdp() {
        override fun onSetFailure(error: String?) {
          super.onSetFailure(error)
          main.post { if (share === s && !s.connected) failShare() }
        }
      }, SessionDescription(SessionDescription.Type.ANSWER, event.getString("sdp")))
      "ended", "over" -> failShare()
    }
  }

  /** The share's peer connection: the silent microphone, then the screen on a section that also receives, as Chrome offers it. */
  private fun openShare(s: Share): PeerConnection? {
    val f = factory ?: return null
    val screen = captures[SCREEN] ?: return null
    val id = s.id.ifEmpty { "$localId#content" }
    val p = f.createPeerConnection(rtcConfiguration(s.ice, s.relayOnly), shareObserver(s)) ?: return null
    val silence = f.createAudioTrack("$id-a0", f.createAudioSource(MediaConstraints()))
    silence.setEnabled(false)
    val source = f.createVideoSource(true)
    val track = f.createVideoTrack("$id-$SCREEN", source)
    s.pc = p; s.source = source; s.track = track
    val added = runCatching {
      p.addTrack(silence, listOf(id))
      p.addTransceiver(track, RtpTransceiver.RtpTransceiverInit(RtpTransceiver.RtpTransceiverDirection.SEND_RECV, listOf(id)))
    }.isSuccess
    if (!added) return null
    screen.target = source.capturerObserver
    return p
  }

  private fun offerShare(s: Share) {
    val p = s.pc ?: openShare(s) ?: return failShare()
    val failed = { main.post { if (share === s) failShare() } }
    p.createOffer(object : Sdp() {
      override fun onCreateSuccess(offer: SessionDescription) {
        p.setLocalDescription(object : Sdp() {
          override fun onSetFailure(error: String?) { super.onSetFailure(error); failed() }
          override fun onSetSuccess() {
            main.post {
              if (closed || share !== s) return@post
              s.offering = true
              if (s.connected) return@post sendShareOffer(s)
              main.postDelayed({ sendShareOffer(s) }, GATHERING_MS)
              if (p.iceGatheringState() == PeerConnection.IceGatheringState.COMPLETE) sendShareOffer(s)
            }
          }
        }, offer)
      }
      override fun onCreateFailure(error: String?) { super.onCreateFailure(error); failed() }
    }, MediaConstraints())
  }

  private fun sendShareOffer(s: Share) {
    if (closed || share !== s || !s.offering) return
    val sdp = s.pc?.localDescription?.description ?: return
    s.offering = false
    if (handle != 0L) JitsiCall.shareOffer(handle, sdp)
  }

  private fun shareObserver(s: Share) = object : PeerConnection.Observer {
    override fun onIceCandidate(c: IceCandidate) {
      if (s.relayOnly) main.postDelayed({ sendShareOffer(s) }, FIRST_CANDIDATE_MS)
    }
    override fun onConnectionChange(state: PeerConnection.PeerConnectionState) {
      main.post {
        if (closed || share !== s) return@post
        when (state) {
          PeerConnection.PeerConnectionState.CONNECTED -> if (!s.connected) {
            s.connected = true
            shared = true
            syncVideo()
          }
          PeerConnection.PeerConnectionState.FAILED -> failShare()
          else -> {}
        }
      }
    }
    override fun onIceGatheringChange(state: PeerConnection.IceGatheringState) {
      if (state == PeerConnection.IceGatheringState.COMPLETE) main.post { sendShareOffer(s) }
    }
    override fun onSignalingChange(state: PeerConnection.SignalingState) {}
    override fun onIceConnectionChange(state: PeerConnection.IceConnectionState) {}
    override fun onIceConnectionReceivingChange(receiving: Boolean) {}
    override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>) {}
    override fun onAddStream(stream: MediaStream) {}
    override fun onRemoveStream(stream: MediaStream) {}
    override fun onDataChannel(channel: DataChannel) {}
    override fun onRenegotiationNeeded() {}
    override fun onAddTrack(receiver: RtpReceiver, streams: Array<out MediaStream>) {}
  }

  /** The screen's session left and its peer connection closed; the capture goes on. */
  private fun closeShare() {
    val s = share ?: return
    share = null
    shared = false
    if (handle != 0L) JitsiCall.share(handle, false)
    s.close()
  }

  /** The screen's session failed: the capture stops and the call goes on, told unavailable when nothing was sent yet. */
  private fun failShare() {
    val s = share ?: return
    val started = s.connected
    closeShare()
    captures.remove(SCREEN)?.let { it.stop(); onScreenStopped?.invoke() }
    if (!started) onScreenUnavailable?.invoke()
    syncVideo()
  }

  /** Called when the screen's capture ends without being asked (Android ended the projection, the provider refused it). */
  var onScreenStopped: (() -> Unit)? = null

  /** Films with the front camera, or stops; false when the provider does not take it or no camera opens. */
  fun setCamera(on: Boolean): Boolean {
    if (!on) {
      captures.remove(CAMERA)?.stop()
      syncVideo()
      return false
    }
    if (!canCamera) return false
    if (captures[CAMERA] == null) captures[CAMERA] = LocalCapture.camera(app) ?: return false
    syncVideo()
    return true
  }

  /** Shares the screen from the user's consent; false when the provider does not take it or the capture fails. */
  fun startScreen(consent: android.content.Intent, width: Int, height: Int, fps: Int): Boolean {
    if (!canScreen) return false
    captures.remove(SCREEN)?.stop()
    captures[SCREEN] = LocalCapture.screen(app, consent, width, height, fps) {
      main.post { if (!closed && captures.remove(SCREEN)?.also { it.stop() } != null) { syncVideo(); onScreenStopped?.invoke() } }
    } ?: return false
    syncVideo()
    return true
  }

  fun stopScreen() {
    captures.remove(SCREEN)?.stop()
    syncVideo()
  }

  /**
   * The remote tracks as the provider names them, by track id: whose and what.
   * A track it does not name is its stream's camera until it names any.
   */
  private fun shown(): Map<String, RemoteVideo> = remoteVideos.mapNotNull { (id, value) ->
    val stream = value.first
    val known = videos.firstOrNull { it.track == id } ?: videos.firstOrNull { it.track == stream } ?: named[id]
    when {
      known != null -> id to known
      !listed -> id to RemoteVideo(id, stream, CAMERA)
      else -> null
    }
  }.toMap()

  /** Someone's camera or screen, this side's included; null while it is not sent. */
  fun videoFeed(identity: String, screen: Boolean): VideoFeed? {
    val kind = if (screen) SCREEN else CAMERA
    if (identity == localId) return captures[kind]
    pcmVideo?.let { video ->
      return if (!screen && video.showing && identity == roster.keys.firstOrNull()) video.feed else null
    }
    val id = shown().entries.firstOrNull { it.value.member == identity && it.value.kind == kind }?.key ?: return null
    return remoteVideos[id]?.second?.let { RtcFeed(it) }
  }

  fun localIdentity(): String = localId

  /**
   * The provider's offer set and answered. An offer of this side's still waiting
   * for its answer is rolled back first: this side yields, and offers again once
   * it answered.
   */
  private fun offer(sdp: String) {
    val p = peer() ?: return onEnd("connect_failed")
    val yielded = p.signalingState() == PeerConnection.SignalingState.HAVE_LOCAL_OFFER
    val answer = {
      p.setRemoteDescription(object : Sdp() {
        override fun onSetSuccess() {
          p.createAnswer(object : Sdp() {
            override fun onCreateSuccess(answer: SessionDescription) {
              p.setLocalDescription(object : Sdp() {
                override fun onSetSuccess() {
                  main.post {
                    if (closed || pc !== p) return@post
                    if (handle != 0L) JitsiCall.answer(handle, answer.description)
                    if (yielded) { offering = false; reoffer() }
                    syncVideo()
                  }
                }
              }, answer)
            }
          }, MediaConstraints())
        }
      }, SessionDescription(SessionDescription.Type.OFFER, sdp))
    }
    if (!yielded) return answer()
    offering = false
    p.setLocalDescription(object : Sdp() {
      override fun onSetSuccess() { main.post { if (pc === p) answer() } }
      override fun onSetFailure(error: String?) { super.onSetFailure(error); main.post { if (pc === p) answer() } }
    }, SessionDescription(SessionDescription.Type.ROLLBACK, ""))
  }

  /**
   * The provider's answer to this side's offer. One crossing an offer of the
   * provider's that this side yielded to comes too late and is dropped; one that
   * cannot be set on a running call is answered by offering again, a few times.
   */
  private fun remoteAnswerOfProvider(sdp: String) {
    val p = pc ?: return
    if (connected && p.signalingState() == PeerConnection.SignalingState.STABLE) return
    p.setRemoteDescription(object : Sdp() {
      override fun onSetSuccess() { main.post { if (pc === p) { reoffers = 0; syncVideo() } } }
      override fun onSetFailure(error: String?) {
        super.onSetFailure(error)
        main.post {
          if (closed || pc !== p) return@post
          if (connected && reoffers < REOFFERS) { reoffers++; reoffer() } else lost = "connect_failed"
        }
      }
    }, SessionDescription(SessionDescription.Type.ANSWER, sdp))
  }

  /** This side's offer anew, the one waiting rolled back first. */
  private fun reoffer() {
    val p = pc ?: return
    if (p.signalingState() != PeerConnection.SignalingState.HAVE_LOCAL_OFFER) return localOffer()
    p.setLocalDescription(object : Sdp() {
      override fun onSetSuccess() { main.post { if (pc === p) localOffer() } }
    }, SessionDescription(SessionDescription.Type.ROLLBACK, ""))
  }

  /** The local offer made and set; it goes out once gathering completed, or after a moment. */
  private fun localOffer() {
    val p = peer() ?: return onEnd("connect_failed")
    addReceivers(p)
    making = true
    val failed = { main.post { if (pc === p) { making = false; deferred = external } } }
    p.createOffer(object : Sdp() {
      override fun onCreateSuccess(offer: SessionDescription) {
        p.setLocalDescription(object : Sdp() {
          override fun onSetFailure(error: String?) { super.onSetFailure(error); failed() }
          override fun onSetSuccess() {
            main.post {
              if (pc === p) making = false
              offering = true
              // A renegotiation gathers nothing new: waiting would only leave room for the provider's own offer to cross it.
              if (connected) return@post sendOffer()
              main.postDelayed({ sendOffer() }, GATHERING_MS)
              if (p.iceGatheringState() == PeerConnection.IceGatheringState.COMPLETE) sendOffer()
            }
          }
        }, offer)
      }
      override fun onCreateFailure(error: String?) { super.onCreateFailure(error); failed() }
    }, MediaConstraints().apply {
      mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveAudio", "true"))
      // Absent, libwebrtc leaves video sections as they are; "false" would strip receiving from every one.
      if ((external && externalVideo) || receiveVideo) mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveVideo", "true"))
    })
  }

  /** Sends the local description, its gathered candidates in it. */
  private fun sendOffer() {
    if (closed || !offering) return
    val sdp = pc?.localDescription?.description ?: return
    offering = false
    if (external) onLocalOffer?.invoke(sdp)
    else if (handle != 0L) JitsiCall.offer(handle, sdp)
  }

  private val observer = object : PeerConnection.Observer {
    override fun onIceCandidate(c: IceCandidate) {
      if (handle != 0L) JitsiCall.candidate(handle, c.sdpMid ?: "", c.sdp)
      // One relay candidate is all a relayed call needs: gathering may take long to say it is done.
      if (relayOnly) main.postDelayed({ sendOffer() }, FIRST_CANDIDATE_MS)
    }
    override fun onConnectionChange(state: PeerConnection.PeerConnectionState) {
      main.post {
        if (closed) return@post
        when (state) {
          PeerConnection.PeerConnectionState.CONNECTED -> {
            connected = true; everConnected = true; reconnecting = false; onChange()
          }
          PeerConnection.PeerConnectionState.FAILED -> if (external) onEnd("connection_failed") else lost = "connection_failed"
          else -> {}
        }
      }
    }
    override fun onAddTrack(receiver: RtpReceiver, streams: Array<out MediaStream>) {
      (receiver.track() as? VideoTrack)?.let { video ->
        val id = video.id()
        val stream = streams.firstOrNull()?.id ?: ""
        main.post {
          if (closed) return@post
          remoteVideos[id] = stream to video
          if (handle != 0L) JitsiCall.remoteVideo(handle, id, stream)
          onChange()
        }
        return
      }
      val track = receiver.track() as? AudioTrack ?: return
      val endpoint = streams.firstOrNull()?.id ?: return
      mixed = endpoint
      track.setEnabled(!deafened)
      tracks[endpoint] = track
    }
    override fun onRemoveTrack(receiver: RtpReceiver) {
      val id = runCatching { receiver.track()?.id() }.getOrNull() ?: return
      main.post { if (remoteVideos.remove(id) != null) onChange() }
    }
    override fun onSignalingChange(state: PeerConnection.SignalingState) {}
    override fun onIceConnectionChange(state: PeerConnection.IceConnectionState) {}
    override fun onIceConnectionReceivingChange(receiving: Boolean) {}
    override fun onIceGatheringChange(state: PeerConnection.IceGatheringState) {
      if (state == PeerConnection.IceGatheringState.COMPLETE) main.post { sendOffer() }
    }
    override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>) {}
    override fun onAddStream(stream: MediaStream) {}
    override fun onRemoveStream(stream: MediaStream) {}
    override fun onDataChannel(channel: DataChannel) {}
    override fun onRenegotiationNeeded() {}
  }

  /** Reads who is heard, about three times a second, from the receivers' audio levels. */
  fun poll() {
    val p = pc ?: return
    p.getStats { report ->
      val byTrack = tracks.entries.associate { it.value.id() to it.key }
      report.statsMap.values.filter { it.type == "inbound-rtp" && it.members["kind"] == "audio" }.forEach { s ->
        val endpoint = byTrack[s.members["trackIdentifier"] as? String] ?: return@forEach
        val level = (s.members["audioLevel"] as? Number)?.toDouble() ?: 0.0
        levels[endpoint] = level
        if (level > SPEAKING) heard[endpoint] = SystemClock.elapsedRealtime()
      }
    }
  }

  fun members(): List<Map<String, Any?>> {
    val now = SystemClock.elapsedRealtime()
    val local = mapOf(
      "identity" to localId, "speaking" to false, "muted" to (!VoiceEngine.microphone || VoiceEngine.deafened),
      "deafened" to VoiceEngine.deafened, "level" to 0.0, "local" to true,
      "camera" to (captures[CAMERA] != null), "screen" to (captures[SCREEN] != null),
    )
    val sent = shown().values
    val peerPicture = pcmVideo?.showing == true
    return listOf(local) + roster.map { (id, value) ->
      // One mixed stream and one person: its loudness is theirs.
      val source = if (roster.size == 1 && heard[id] == null && levels[id] == null) mixed ?: id else id
      mapOf(
        "identity" to id, "name" to value.first, "muted" to value.second, "deafened" to false,
        "speaking" to (!value.second && (heard[source]?.let { now - it < HANGOVER_MS } ?: false)),
        "level" to (levels[source] ?: 0.0), "local" to false,
        "camera" to (peerPicture || sent.any { it.member == id && it.kind == CAMERA }), "screen" to sent.any { it.member == id && it.kind == SCREEN },
      )
    }
  }

  fun setMicrophone(on: Boolean) {
    microphone?.setEnabled(on)
    if (handle != 0L) JitsiCall.muted(handle, !on)
  }

  fun setDeafened(on: Boolean) {
    deafened = on
    tracks.values.forEach { it.setEnabled(!on) }
  }

  /** Hangs up; the reader thread leaves the conference within half a second. */
  fun close() {
    if (closed) return
    closed = true
    runCatching { pcm?.close() }
    pcmVideo?.close()
    dropMedia()
    captures.values.forEach { it.stop() }
    captures.clear()
  }

  /** One session's media: the peer connection, its tracks, who was heard. */
  private fun dropMedia() {
    share?.close(); share = null; shared = false
    channel?.dispose(); channel = null
    outbox.clear()
    captures.values.forEach { it.target = null }
    pc?.close(); pc?.dispose(); pc = null
    localTracks.values.forEach { (source, track) -> track.dispose(); source.dispose() }
    localTracks.clear(); senders.clear(); told = false to false; reoffers = 0; making = false; deferred = false
    addedReceivers = 0
    remoteVideos.clear(); videos = emptyList(); named.clear(); listed = false
    microphone = null
    factory?.dispose(); factory = null
    audio?.release(); audio = null
    tracks.clear(); heard.clear(); levels.clear(); roster.clear()
    offering = false
  }

  private open class Sdp : SdpObserver {
    override fun onCreateSuccess(description: SessionDescription) {}
    override fun onSetSuccess() {}
    override fun onCreateFailure(error: String?) { Log.w("RocketVibeVoice", "jitsi sdp: $error") }
    override fun onSetFailure(error: String?) { Log.w("RocketVibeVoice", "jitsi sdp: $error") }
  }

  private companion object {
    const val SPEAKING = 0.02
    /** The loudness, as the provider reports it, above which a member speaks. */
    const val REPORTED_SPEAKING = 0.15
    const val GATHERING_MS = 5000L
    const val FIRST_CANDIDATE_MS = 500L
    const val REOFFERS = 3
    const val CAMERA = "camera"
    const val SCREEN = "screen"
    const val HANGOVER_MS = 350L
    /** The pause before each rejoin; as many tries. */
    val REJOIN_DELAYS_S = longArrayOf(1, 2, 4, 8, 15)
  }
}

/** A remote video as the provider names it: its track (or stream) id, whose, `camera` or `screen`. */
internal data class RemoteVideo(val track: String, val member: String, val kind: String)
