package com.rocketvibe.voice

import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjection
import livekit.org.webrtc.Camera2Enumerator
import livekit.org.webrtc.CapturerObserver
import livekit.org.webrtc.EglBase
import livekit.org.webrtc.ScreenCapturerAndroid
import livekit.org.webrtc.SurfaceTextureHelper
import livekit.org.webrtc.VideoCapturer
import livekit.org.webrtc.VideoFrame
import livekit.org.webrtc.VideoSink
import livekit.org.webrtc.VideoTrack
import java.util.concurrent.CopyOnWriteArraySet

/** Frames a [VoiceVideoView] draws: a LiveKit room's track, a native call's, this side's capture. */
internal interface VideoFeed {
  fun addSink(sink: VideoSink)
  fun removeSink(sink: VideoSink)
}

internal data class RoomFeed(val track: io.livekit.android.room.track.VideoTrack) : VideoFeed {
  override fun addSink(sink: VideoSink) = track.addRenderer(sink)
  override fun removeSink(sink: VideoSink) = track.removeRenderer(sink)
}

/** A native call's remote track; gone with its peer connection, after which removing a sink throws. */
internal data class RtcFeed(val track: VideoTrack) : VideoFeed {
  override fun addSink(sink: VideoSink) { runCatching { track.addSink(sink) } }
  override fun removeSink(sink: VideoSink) { runCatching { track.removeSink(sink) } }
}

internal object NativeVideo {
  /** One EGL context for every native call: their decoders, captures and views share textures through it. */
  val egl: EglBase by lazy { EglBase.create() }
}

/**
 * This side's camera or screen in a native call. It outlives a media session:
 * its frames go to [target], the video source of the current peer
 * connection's factory, and to the previews drawing it.
 */
internal class LocalCapture private constructor(
  private val capturer: VideoCapturer,
  private val helper: SurfaceTextureHelper,
) : CapturerObserver, VideoFeed {
  @Volatile private var started = false
  private val sinks = CopyOnWriteArraySet<VideoSink>()

  @Volatile var target: CapturerObserver? = null
    set(value) {
      field = value
      if (started) value?.onCapturerStarted(true)
    }

  override fun onCapturerStarted(success: Boolean) {
    started = success
    target?.onCapturerStarted(success)
  }

  override fun onCapturerStopped() {
    started = false
    target?.onCapturerStopped()
  }

  override fun onFrameCaptured(frame: VideoFrame) {
    target?.onFrameCaptured(frame)
    sinks.forEach { it.onFrame(frame) }
  }

  override fun addSink(sink: VideoSink) { sinks.add(sink) }
  override fun removeSink(sink: VideoSink) { sinks.remove(sink) }

  fun stop() {
    target = null
    runCatching { capturer.stopCapture() }
    capturer.dispose()
    helper.dispose()
    sinks.clear()
  }

  companion object {
    private fun start(app: Context, name: String, capturer: VideoCapturer, width: Int, height: Int, fps: Int): LocalCapture? {
      val helper = SurfaceTextureHelper.create(name, NativeVideo.egl.eglBaseContext) ?: return null.also { capturer.dispose() }
      val capture = LocalCapture(capturer, helper)
      return runCatching {
        capturer.initialize(helper, app, capture)
        capturer.startCapture(width, height, fps)
        capture
      }.getOrElse {
        capturer.dispose()
        helper.dispose()
        null
      }
    }

    /** The front camera, else the first one. */
    fun camera(app: Context): LocalCapture? {
      val cameras = Camera2Enumerator(app)
      val name = cameras.deviceNames.firstOrNull { cameras.isFrontFacing(it) } ?: cameras.deviceNames.firstOrNull() ?: return null
      val capturer = cameras.createCapturer(name, null) ?: return null
      return start(app, "camera", capturer, 1280, 720, 30)
    }

    /** The screen, from the user's MediaProjection consent; [onStop] when Android ends the projection. */
    fun screen(app: Context, consent: Intent, width: Int, height: Int, fps: Int, onStop: () -> Unit): LocalCapture? {
      val capturer = ScreenCapturerAndroid(consent, object : MediaProjection.Callback() {
        override fun onStop() = onStop()
      })
      return start(app, "screen", capturer, width, height, fps)
    }
  }
}
