package com.rocketvibe.voice

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.media.audiofx.NoiseSuppressor
import android.os.SystemClock
import kotlin.math.log10
import kotlin.math.sqrt

/**
 * When to remind someone speaking with the microphone off, as the desktop and
 * web apps do (`rv_voice_protocol::MutedSpeechReminder`): speech lasting 600 ms,
 * once per burst, at most every 10 s.
 */
internal class MutedSpeechReminder {
  private var began: Long? = null
  private var warned = false
  private var last: Long? = null

  fun update(muted: Boolean, speaking: Boolean, now: Long): Boolean {
    if (!muted || !speaking) {
      began = null
      warned = false
      return false
    }
    val start = began ?: now.also { began = it }
    val rested = last?.let { now - it >= 10_000 } ?: true
    if (!warned && now - start >= 600 && rested) {
      warned = true
      last = now
      return true
    }
    return false
  }
}

/**
 * While the microphone is off in a voice room (LiveKit then stops capturing): a
 * recorder of its own, local only, connected to nothing, that only tells when the
 * user speaks anyway. [onSpeech] is called on its thread.
 */
internal class MutedMonitor(private val onSpeech: () -> Unit) {
  @Volatile private var running = false
  private var thread: Thread? = null
  private val reminder = MutedSpeechReminder()

  @SuppressLint("MissingPermission")
  fun start() {
    if (running) return
    running = true
    thread = Thread({
      val rate = 16_000
      val samples = 320
      val buffer = maxOf(AudioRecord.getMinBufferSize(rate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT), samples * 4)
      val record = runCatching {
        AudioRecord(MediaRecorder.AudioSource.VOICE_COMMUNICATION, rate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, buffer)
      }.getOrNull()
      if (record == null || record.state != AudioRecord.STATE_INITIALIZED) {
        record?.release()
        running = false
        return@Thread
      }
      val noise = if (NoiseSuppressor.isAvailable()) NoiseSuppressor.create(record.audioSessionId) else null
      noise?.enabled = true
      record.startRecording()
      val frame = ShortArray(samples)
      while (running) {
        val n = record.read(frame, 0, samples)
        if (n <= 0) break
        var sum = 0.0
        for (i in 0 until n) sum += frame[i].toDouble() * frame[i]
        val rms = sqrt(sum / n) / 32_768.0
        val db = if (rms > 0) 20 * log10(rms) else -120.0
        if (reminder.update(true, db > SPEECH_DB, SystemClock.elapsedRealtime())) onSpeech()
      }
      runCatching { record.stop() }
      record.release()
      noise?.release()
    }, "muted-speech").also { it.start() }
  }

  fun stop() {
    running = false
    thread = null
  }

  private companion object {
    /** The desktop sidecar's threshold without its voice detector (`LOCAL_SPEECH_DB`). */
    const val SPEECH_DB = -50.0
  }
}
