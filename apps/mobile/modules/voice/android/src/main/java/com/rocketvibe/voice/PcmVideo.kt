package com.rocketvibe.voice

import android.media.Image
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import livekit.org.webrtc.JavaI420Buffer
import livekit.org.webrtc.SurfaceTextureHelper
import livekit.org.webrtc.VideoFrame
import livekit.org.webrtc.VideoSink
import livekit.org.webrtc.YuvHelper
import java.io.DataInputStream
import java.io.EOFException
import java.io.IOException
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.CopyOnWriteArraySet
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The WhatsApp engine's video connection (crates/rv-whatsapp/engine/src/call_video.rs):
 * messages of one kind byte, a little-endian `u32` length and the payload, H.264
 * access units in Annex-B both ways.
 */
internal object PcmVideoWire {
  const val UP_PICTURE = 1
  const val UP_SENDING = 2
  const val UP_KEYFRAME = 3
  const val DOWN_PICTURE = 1
  const val DOWN_PEER = 2
  const val DOWN_KEYFRAME = 3
  const val DOWN_REFUSED = 4
  const val MAX_MESSAGE = 4 shl 20

  /** What goes out at most, landscape; an upright picture fits it turned. */
  const val FIT_LONG = 1280
  const val FIT_SHORT = 720

  fun message(kind: Int, payload: ByteArray): ByteArray {
    val message = ByteArray(5 + payload.size)
    message[0] = kind.toByte()
    putLe(message, 1, payload.size)
    payload.copyInto(message, 5)
    return message
  }

  fun picture(timestamp: Int, unit: ByteArray): ByteArray {
    val payload = ByteArray(4 + unit.size)
    putLe(payload, 0, timestamp)
    unit.copyInto(payload, 4)
    return message(UP_PICTURE, payload)
  }

  fun sending(on: Boolean): ByteArray = message(UP_SENDING, byteArrayOf(if (on) 1 else 0))

  /** The next message as kind and payload; null when the stream ended. */
  fun read(input: DataInputStream): Pair<Int, ByteArray>? {
    val head = ByteArray(5)
    try {
      input.readFully(head)
    } catch (_: EOFException) {
      return null
    }
    val length = getLe(head, 1).toLong() and 0xffffffffL
    if (length > MAX_MESSAGE) throw IOException("video message of $length bytes")
    val payload = ByteArray(length.toInt())
    input.readFully(payload)
    return (head[0].toInt() and 0xff) to payload
  }

  data class PeerPicture(val keyframe: Boolean, val turns: Int, val timestamp: Int, val unit: ByteArray)

  fun peerPicture(payload: ByteArray): PeerPicture? {
    if (payload.size <= 6) return null
    return PeerPicture(payload[0].toInt() != 0, (payload[1].toInt() and 0xff) % 4, getLe(payload, 2), payload.copyOfRange(6, payload.size))
  }

  /** An encoder's access unit as it goes out: a keyframe led by the parameter sets, unless it carries them. */
  fun accessUnit(unit: ByteArray, keyframe: Boolean, parameterSets: ByteArray?): ByteArray =
    if (keyframe && parameterSets != null && parameterSets.isNotEmpty() && !carriesParameterSets(unit)) parameterSets + unit else unit

  /** Whether an Annex-B access unit has a sequence parameter set (NAL type 7). */
  fun carriesParameterSets(unit: ByteArray): Boolean {
    var i = 0
    while (i + 3 < unit.size) {
      if (unit[i].toInt() == 0 && unit[i + 1].toInt() == 0) {
        val start = when {
          unit[i + 2].toInt() == 1 -> i + 3
          unit[i + 2].toInt() == 0 && unit[i + 3].toInt() == 1 -> i + 4
          else -> -1
        }
        if (start in 0 until unit.size) {
          if (unit[start].toInt() and 0x1f == 7) return true
          i = start
          continue
        }
      }
      i++
    }
    return false
  }

  /** An upright picture's size as sent: within 1280x720 (720x1280 standing), never larger, in whole macroblocks. */
  fun fitted(width: Int, height: Int): Pair<Int, Int> {
    val w = maxOf(width, 16)
    val h = maxOf(height, 16)
    val (boxW, boxH) = if (w >= h) FIT_LONG to FIT_SHORT else FIT_SHORT to FIT_LONG
    val scale = minOf(boxW.toDouble() / w, boxH.toDouble() / h, 1.0)
    val block = { v: Double -> maxOf((v.toInt() / 16) * 16, 16) }
    return block(w * scale) to block(h * scale)
  }

  /** The 90 kHz clock of a presentation time in microseconds, wrapping as a `u32`. */
  fun clock(microseconds: Long): Int = (microseconds * 9 / 100).toInt()

  private fun putLe(bytes: ByteArray, at: Int, value: Int) {
    for (i in 0 until 4) bytes[at + i] = (value ushr (8 * i)).toByte()
  }

  private fun getLe(bytes: ByteArray, at: Int): Int =
    (0 until 4).fold(0) { acc, i -> acc or ((bytes[at + i].toInt() and 0xff) shl (8 * i)) }
}

/** The peer's decoded pictures, drawn by the views that show them. */
internal class DecodedFeed : VideoFeed {
  private val sinks = CopyOnWriteArraySet<VideoSink>()
  override fun addSink(sink: VideoSink) { sinks.add(sink) }
  override fun removeSink(sink: VideoSink) { sinks.remove(sink) }
  fun deliver(frame: VideoFrame) = sinks.forEach { it.onFrame(frame) }
  fun clear() = sinks.clear()
}

/**
 * A WhatsApp call's video beside its audio: the engine's second connection
 * (`<token> video`). This side's camera or screen goes out as H.264 from the
 * phone's encoder, the screen while it is shared, else the camera; the peer's
 * pictures are decoded onto a texture, turned as they say, and shown as their
 * camera. [onChange] and [onRefused] are called on a worker thread.
 */
internal class PcmVideo private constructor(
  private val socket: Socket,
  private val onChange: () -> Unit,
  private val onRefused: () -> Unit,
) {
  @Volatile private var closed = false
  private val outbox = LinkedBlockingQueue<ByteArray>()
  private val pending = ArrayBlockingQueue<Pair<VideoFrame.I420Buffer, Boolean>>(1)
  private val keyframe = AtomicBoolean(false)
  @Volatile private var tapped: LocalCapture? = null
  @Volatile private var tap: VideoSink? = null
  @Volatile private var tappedScreen = false
  @Volatile private var lastTap = 0L
  private val started = SystemClock.elapsedRealtimeNanos()

  val feed = DecodedFeed()
  /** The peer sends and their pictures show. */
  @Volatile var showing = false; private set
  @Volatile private var peerOn = false

  private fun start() {
    Thread({ write() }, "pcm-video-out").start()
    Thread({ receive() }, "pcm-video-in").start()
    Thread({ encode() }, "pcm-video-encoder").start()
  }

  /**
   * What goes out: [capture] (the screen when [screen]), or nothing. The
   * engine is told when this side starts or stops sending.
   */
  fun send(capture: LocalCapture?, screen: Boolean) {
    if (closed || (capture === tapped && screen == tappedScreen)) return
    val was = tapped != null
    tap?.let { tapped?.removeSink(it) }
    tapped = capture
    tappedScreen = screen
    tap = capture?.let { c ->
      VideoSink { frame -> captured(frame, screen) }.also { c.addSink(it) }
    }
    keyframe.set(true)
    if (was != (capture != null)) outbox.add(PcmVideoWire.sending(capture != null))
  }

  fun close() {
    if (closed) return
    closed = true
    tap?.let { tapped?.removeSink(it) }
    tapped = null; tap = null
    runCatching { socket.close() }
    outbox.add(ByteArray(0))
  }

  /** On the capture's thread: one picture every [INTERVAL_MS] at most, fitted and upright, for the encoder. */
  private fun captured(frame: VideoFrame, screen: Boolean) {
    val now = SystemClock.elapsedRealtime()
    if (closed || now - lastTap < INTERVAL_MS || pending.remainingCapacity() == 0) return
    lastTap = now
    val buffer = frame.buffer
    val (width, height) = buffer.width to buffer.height
    val turned = frame.rotation % 180 != 0
    val (uprightW, uprightH) = PcmVideoWire.fitted(if (turned) height else width, if (turned) width else height)
    val (w, h) = if (turned) uprightH to uprightW else uprightW to uprightH
    val cropW = minOf(width, (height.toLong() * w / h).toInt()) and 1.inv()
    val cropH = minOf(height, (width.toLong() * h / w).toInt()) and 1.inv()
    val scaled = buffer.cropAndScale(((width - cropW) / 2) and 1.inv(), ((height - cropH) / 2) and 1.inv(), cropW, cropH, w, h)
    val i420 = try { scaled.toI420() } finally { scaled.release() } ?: return
    val upright = if (frame.rotation == 0) i420 else try { rotated(i420, frame.rotation) } finally { i420.release() }
    if (!pending.offer(upright to screen)) upright.release()
  }

  private fun rotated(source: VideoFrame.I420Buffer, degrees: Int): VideoFrame.I420Buffer {
    val turned = degrees % 180 != 0
    val target = JavaI420Buffer.allocate(if (turned) source.height else source.width, if (turned) source.width else source.height)
    YuvHelper.I420Rotate(
      source.dataY, source.strideY, source.dataU, source.strideU, source.dataV, source.strideV,
      target.dataY, target.strideY, target.dataU, target.strideU, target.dataV, target.strideV,
      source.width, source.height, degrees,
    )
    return target
  }

  private fun write() {
    val output = runCatching { socket.getOutputStream() }.getOrNull() ?: return
    while (!closed) {
      val message = outbox.take()
      if (message.isEmpty() || runCatching { output.write(message) }.isFailure) break
    }
  }

  /** The encoder's thread: the pictures as access units, a keyframe when one is asked for. */
  private fun encode() {
    var encoder: H264Encoder? = null
    val send = { presentationUs: Long, unit: ByteArray -> outbox.add(PcmVideoWire.picture(PcmVideoWire.clock(presentationUs), unit)); Unit }
    try {
      while (!closed) {
        val next = pending.poll(100, TimeUnit.MILLISECONDS)
        if (next == null) {
          if (tapped == null) { encoder?.release(); encoder = null }
          runCatching { encoder?.drain(send, 0) }
          continue
        }
        val (picture, screen) = next
        try {
          val current = encoder?.takeIf { it.fits(picture.width, picture.height, screen) } ?: run {
            encoder?.release()
            keyframe.set(false)
            H264Encoder.create(picture.width, picture.height, screen).also { encoder = it }
          }
          if (current == null) { Log.w(TAG, "pcm video: no H.264 encoder"); continue }
          if (keyframe.getAndSet(false)) current.keyframe()
          current.encode(picture, (SystemClock.elapsedRealtimeNanos() - started) / 1000, send)
        } catch (error: Exception) {
          Log.w(TAG, "pcm video: encoder: $error")
          encoder?.release(); encoder = null
        } finally {
          picture.release()
        }
      }
    } finally {
      encoder?.release()
      while (true) pending.poll()?.first?.release() ?: break
    }
  }

  /** The reader's thread: the peer's pictures decoded and shown, the engine's news. */
  private fun receive() {
    val helper = SurfaceTextureHelper.create("pcm-video-peer", NativeVideo.egl.eglBaseContext)
    if (helper == null) { close(); return }
    helper.setTextureSize(PcmVideoWire.FIT_LONG, PcmVideoWire.FIT_SHORT)
    val surface = Surface(helper.surfaceTexture)
    helper.startListening { frame ->
      if (!peerOn) return@startListening
      feed.deliver(frame)
      if (!showing) { showing = true; onChange() }
    }
    var decoder: H264Decoder? = null
    var waitingKeyframe = true
    var asked = 0L
    var turns = -1
    val askKeyframe = {
      val now = SystemClock.elapsedRealtime()
      if (now - asked >= KEYFRAME_ASK_MS) {
        asked = now
        outbox.add(PcmVideoWire.message(PcmVideoWire.UP_KEYFRAME, ByteArray(0)))
      }
    }
    try {
      val input = DataInputStream(socket.getInputStream().buffered())
      while (!closed) {
        val (kind, payload) = PcmVideoWire.read(input) ?: break
        when (kind) {
          PcmVideoWire.DOWN_PICTURE -> {
            val picture = PcmVideoWire.peerPicture(payload) ?: continue
            peerOn = true
            if (waitingKeyframe && !picture.keyframe) { askKeyframe(); continue }
            if (picture.turns != turns) { turns = picture.turns; helper.setFrameRotation(turns * 90) }
            try {
              val d = decoder ?: H264Decoder(surface) { w, h -> helper.setTextureSize(w, h) }.also { decoder = it }
              waitingKeyframe = false
              if (!d.decode(picture.unit, (picture.timestamp.toLong() and 0xffffffffL) * 100 / 9)) {
                waitingKeyframe = true
                askKeyframe()
              }
            } catch (error: Exception) {
              Log.w(TAG, "pcm video: decoder: $error")
              decoder?.release(); decoder = null
              waitingKeyframe = true
              askKeyframe()
            }
          }
          PcmVideoWire.DOWN_PEER -> if (payload.firstOrNull()?.toInt() == 0) {
            peerOn = false
            waitingKeyframe = true
            if (showing) { showing = false; onChange() }
          }
          PcmVideoWire.DOWN_KEYFRAME -> keyframe.set(true)
          PcmVideoWire.DOWN_REFUSED -> onRefused()
        }
      }
    } catch (error: IOException) {
      if (!closed) Log.w(TAG, "pcm video: $error")
    } finally {
      peerOn = false
      decoder?.release()
      helper.stopListening()
      surface.release()
      helper.dispose()
      feed.clear()
      if (showing) { showing = false; onChange() }
      close()
    }
  }

  companion object {
    private const val TAG = "RocketVibeVoice"
    private const val INTERVAL_MS = 66L
    private const val KEYFRAME_ASK_MS = 1_000L

    /** Opens the video connection of the call at `host:port`; null when the engine takes none. */
    fun open(host: String, port: Int, token: String, onChange: () -> Unit, onRefused: () -> Unit): PcmVideo? {
      val socket = runCatching {
        Socket().apply {
          tcpNoDelay = true
          connect(InetSocketAddress(host, port), 5_000)
          getOutputStream().apply { write("$token video\n".toByteArray()); flush() }
        }
      }.getOrNull() ?: return null
      return PcmVideo(socket, onChange, onRefused).also { it.start() }
    }
  }
}

/** The phone's H.264 encoder: Constrained Baseline where it offers it, else Baseline, YUV 4:2:0 in. */
internal class H264Encoder private constructor(
  private val codec: MediaCodec,
  private val width: Int,
  private val height: Int,
  private val screen: Boolean,
) {
  private val info = MediaCodec.BufferInfo()
  private var parameterSets: ByteArray? = null

  fun fits(width: Int, height: Int, screen: Boolean) = width == this.width && height == this.height && screen == this.screen

  fun keyframe() {
    codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
  }

  fun encode(picture: VideoFrame.I420Buffer, presentationUs: Long, out: (Long, ByteArray) -> Unit) {
    val index = codec.dequeueInputBuffer(INPUT_WAIT_US)
    if (index >= 0) {
      val image = codec.getInputImage(index)
      if (image == null) {
        codec.queueInputBuffer(index, 0, 0, presentationUs, 0)
      } else {
        fill(image, picture)
        codec.queueInputBuffer(index, 0, width * height * 3 / 2, presentationUs, 0)
      }
    }
    drain(out, OUTPUT_WAIT_US)
  }

  /** The access units ready, waiting [timeoutUs] for the first. */
  fun drain(out: (Long, ByteArray) -> Unit, timeoutUs: Long) {
    var wait = timeoutUs
    while (true) {
      val index = codec.dequeueOutputBuffer(info, wait)
      wait = 0
      if (index == MediaCodec.INFO_TRY_AGAIN_LATER) return
      if (index < 0) continue
      val buffer = codec.getOutputBuffer(index)
      val bytes = ByteArray(info.size)
      if (buffer != null) { buffer.position(info.offset); buffer.get(bytes) }
      val flags = info.flags
      val presentation = info.presentationTimeUs
      codec.releaseOutputBuffer(index, false)
      if (flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
        parameterSets = (parameterSets ?: ByteArray(0)) + bytes
        continue
      }
      if (bytes.isEmpty()) continue
      out(presentation, PcmVideoWire.accessUnit(bytes, flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0, parameterSets))
    }
  }

  fun release() {
    runCatching { codec.stop() }
    runCatching { codec.release() }
  }

  private fun fill(image: Image, picture: VideoFrame.I420Buffer) {
    val planes = image.planes
    copyPlane(picture.dataY, picture.strideY, planes[0], width, height)
    copyPlane(picture.dataU, picture.strideU, planes[1], (width + 1) / 2, (height + 1) / 2)
    copyPlane(picture.dataV, picture.strideV, planes[2], (width + 1) / 2, (height + 1) / 2)
  }

  private fun copyPlane(source: ByteBuffer, stride: Int, plane: Image.Plane, w: Int, h: Int) {
    val target = plane.buffer
    val (rowStride, pixelStride) = plane.rowStride to plane.pixelStride
    for (row in 0 until h) {
      val (from, to) = row * stride to row * rowStride
      if (pixelStride == 1) {
        val line = source.duplicate()
        line.limit(from + w).position(from)
        target.position(to)
        target.put(line)
      } else {
        for (column in 0 until w) target.put(to + column * pixelStride, source.get(from + column))
      }
    }
  }

  companion object {
    private const val MIME = MediaFormat.MIMETYPE_VIDEO_AVC
    private const val INPUT_WAIT_US = 10_000L
    private const val OUTPUT_WAIT_US = 20_000L
    private const val CAMERA_BITRATE = 800_000
    private const val SCREEN_BITRATE = 1_200_000
    private const val FPS = 15
    private const val KEYFRAME_EVERY_S = 3

    fun create(width: Int, height: Int, screen: Boolean): H264Encoder? {
      val codec = runCatching { MediaCodec.createEncoderByType(MIME) }.getOrNull() ?: return null
      val profiles = runCatching { codec.codecInfo.getCapabilitiesForType(MIME).profileLevels.map { it.profile } }.getOrDefault(emptyList())
      val profile = when {
        MediaCodecInfo.CodecProfileLevel.AVCProfileConstrainedBaseline in profiles -> MediaCodecInfo.CodecProfileLevel.AVCProfileConstrainedBaseline
        else -> MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline
      }
      val format = { withProfile: Boolean ->
        MediaFormat.createVideoFormat(MIME, width, height).apply {
          setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible)
          setInteger(MediaFormat.KEY_BIT_RATE, if (screen) SCREEN_BITRATE else CAMERA_BITRATE)
          setInteger(MediaFormat.KEY_FRAME_RATE, FPS)
          setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, KEYFRAME_EVERY_S)
          if (withProfile) {
            setInteger(MediaFormat.KEY_PROFILE, profile)
            setInteger(MediaFormat.KEY_LEVEL, MediaCodecInfo.CodecProfileLevel.AVCLevel31)
          }
        }
      }
      val configured = runCatching { codec.configure(format(true), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE) }.isSuccess ||
        runCatching { codec.reset(); codec.configure(format(false), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE) }.isSuccess
      if (!configured || runCatching { codec.start() }.isFailure) {
        runCatching { codec.release() }
        return null
      }
      return H264Encoder(codec, width, height, screen)
    }
  }
}

/** The phone's H.264 decoder, drawing onto [surface]; [onSize] gives the pictures' visible size. */
internal class H264Decoder(surface: Surface, private val onSize: (Int, Int) -> Unit) {
  private val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
  private val info = MediaCodec.BufferInfo()

  init {
    val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, 1280, 720)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
    try {
      codec.configure(format, surface, null, 0)
      codec.start()
    } catch (error: Exception) {
      runCatching { codec.release() }
      throw error
    }
  }

  /** False when the decoder had no room for the unit, which is then lost. */
  fun decode(unit: ByteArray, presentationUs: Long): Boolean {
    val index = codec.dequeueInputBuffer(INPUT_WAIT_US)
    if (index < 0) { drain(); return false }
    val buffer = codec.getInputBuffer(index)
    if (buffer == null || buffer.capacity() < unit.size) {
      codec.queueInputBuffer(index, 0, 0, presentationUs, 0)
      return false
    }
    buffer.clear()
    buffer.put(unit)
    codec.queueInputBuffer(index, 0, unit.size, presentationUs, 0)
    drain()
    return true
  }

  private fun drain() {
    var wait = OUTPUT_WAIT_US
    while (true) {
      val index = codec.dequeueOutputBuffer(info, wait)
      wait = 0
      when {
        index == MediaCodec.INFO_TRY_AGAIN_LATER -> return
        index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
          val format = codec.outputFormat
          val width = if (format.containsKey("crop-right") && format.containsKey("crop-left")) format.getInteger("crop-right") - format.getInteger("crop-left") + 1 else format.getInteger(MediaFormat.KEY_WIDTH)
          val height = if (format.containsKey("crop-bottom") && format.containsKey("crop-top")) format.getInteger("crop-bottom") - format.getInteger("crop-top") + 1 else format.getInteger(MediaFormat.KEY_HEIGHT)
          onSize(width, height)
        }
        index >= 0 -> codec.releaseOutputBuffer(index, true)
      }
    }
  }

  fun release() {
    runCatching { codec.stop() }
    runCatching { codec.release() }
  }

  private companion object {
    const val INPUT_WAIT_US = 20_000L
    const val OUTPUT_WAIT_US = 10_000L
  }
}
