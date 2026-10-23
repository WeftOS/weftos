package com.weftos.glasses

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.os.SystemClock
import android.util.Log
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * Streaming speaker path: PCM16 16 kHz mono into an `AudioTrack`. Replaces the AudioBridge
 * file queue (design 3.5). Cues share the queue with speech so ordering is natural and a
 * later AEC reference can see both.
 *
 * Callbacks run on the playback thread; keep them short.
 */
class Playback(
    private val serial: SerialQueue,
    private val usage: String,
    private val i2sControl: Boolean,
    private val onSpeaking: (Boolean) -> Unit,
    private val onSpeechDrained: (uttId: Int, throughSeq: Int) -> Unit,
) {
    private sealed class Item {
        class Speech(val utt: Int, val seq: Int, val pcm: ByteArray, val enqueuedAt: Long) : Item()
        class Cue(val pcm: ByteArray) : Item()
        class Flush(val utt: Int) : Item()
    }

    private val queue = LinkedBlockingQueue<Item>()

    @Volatile var voiceVolume = 1.0f
    @Volatile var cueVolume = 0.4f

    @Volatile var speaking = false
        private set

    /** Milliseconds from the first reply frame of an utterance to the first write (G1 evidence). */
    @Volatile var lastFirstFrameToWriteMs = -1L
        private set

    @Volatile private var running = true
    private var thread: Thread? = null

    private var track: AudioTrack? = null
    private var i2sOpen = false
    private var lastUtt = -1
    private var lastSeq = -1
    private var speechPending = false
    private var lastWriteAt = 0L

    fun start() {
        if (thread != null) return
        thread = Thread({ loop() }, "glasses-playback").also {
            it.isDaemon = true
            it.start()
        }
    }

    fun stop() {
        running = false
        queue.offer(Item.Flush(-1))
        thread?.join(500)
    }

    fun enqueueSpeech(utt: Int, seq: Int, pcm: ByteArray) {
        queue.offer(Item.Speech(utt, seq, pcm, SystemClock.elapsedRealtime()))
    }

    /** Cue samples arrive pre-gained by the caller's volume choice; see [cueGain]. */
    fun enqueueCue(pcm: ByteArray) {
        queue.offer(Item.Cue(pcm))
    }

    /** Effective gain of a cue: its own volume relative to the voice volume. */
    fun cueGain(): Float = (cueVolume * voiceVolume).coerceIn(0f, 1f)

    fun flush(utt: Int) {
        queue.offer(Item.Flush(utt))
    }

    private fun attrs(): AudioAttributes {
        val u = when (usage) {
            "media" -> AudioAttributes.USAGE_MEDIA
            "assistant" -> AudioAttributes.USAGE_ASSISTANT
            else -> AudioAttributes.USAGE_NOTIFICATION
        }
        return AudioAttributes.Builder()
            .setUsage(u)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
            .build()
    }

    private fun openTrack(): AudioTrack? {
        track?.let { return it }
        return try {
            val fmt = AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setSampleRate(AudioFormatP0.SAMPLE_RATE)
                .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                .build()
            val min = AudioTrack.getMinBufferSize(
                AudioFormatP0.SAMPLE_RATE, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT,
            )
            // About 100 ms of buffering keeps the flush path short.
            val buf = maxOf(min, AudioFormatP0.SAMPLE_RATE / 10 * 2)
            val t = AudioTrack.Builder()
                .setAudioAttributes(attrs())
                .setAudioFormat(fmt)
                .setBufferSizeInBytes(buf)
                .setTransferMode(AudioTrack.MODE_STREAM)
                .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                .build()
            if (t.state != AudioTrack.STATE_INITIALIZED) {
                t.release()
                null
            } else {
                t.play()
                track = t
                t
            }
        } catch (e: Exception) {
            Log.w(TAG, "AudioTrack open failed", e)
            null
        }
    }

    private fun openI2s() {
        if (!i2sControl || i2sOpen) return
        i2sOpen = true
        serial.submit(LedCommands.i2sStart())
        serial.await(500)
        // AudioBridge waits 100 ms after mh_starti2s before the first sample.
        try { Thread.sleep(100) } catch (_: InterruptedException) { }
    }

    private fun closeIdle() {
        track?.let {
            try { it.stop() } catch (_: Exception) { }
            it.release()
        }
        track = null
        if (i2sControl && i2sOpen) serial.submit(LedCommands.i2sStop())
        i2sOpen = false
        setSpeaking(false)
    }

    private fun setSpeaking(on: Boolean) {
        if (speaking != on) {
            speaking = on
            onSpeaking(on)
        }
    }

    private fun write(pcm: ByteArray): Boolean {
        val t = track ?: return false
        var off = 0
        while (off < pcm.size && running) {
            val n = t.write(pcm, off, pcm.size - off) // blocks while the buffer is full
            if (n < 0) {
                Log.w(TAG, "AudioTrack.write error $n")
                return false
            }
            off += n
        }
        return true
    }

    private fun loop() {
        while (running) {
            val item = try {
                queue.poll(DRAIN_MS, TimeUnit.MILLISECONDS)
            } catch (e: InterruptedException) {
                break
            }
            if (item == null) {
                val idle = SystemClock.elapsedRealtime() - lastWriteAt
                if (speechPending && idle >= DRAIN_MS) {
                    speechPending = false
                    setSpeaking(false)
                    onSpeechDrained(lastUtt, lastSeq)
                }
                if (track != null && idle >= IDLE_CLOSE_MS) closeIdle()
                continue
            }
            when (item) {
                is Item.Flush -> {
                    queue.removeAll { it is Item.Speech && (item.utt < 0 || it.utt == item.utt) }
                    track?.let {
                        try {
                            it.pause()
                            it.flush()
                            it.play()
                        } catch (_: Exception) { }
                    }
                    speechPending = false
                    setSpeaking(false)
                }
                is Item.Speech -> {
                    openI2s()
                    if (openTrack() == null) continue
                    if (item.utt != lastUtt) lastFirstFrameToWriteMs = SystemClock.elapsedRealtime() - item.enqueuedAt
                    lastUtt = item.utt
                    lastSeq = item.seq
                    speechPending = true
                    setSpeaking(true)
                    write(Pcm.scale(item.pcm, voiceVolume))
                    lastWriteAt = SystemClock.elapsedRealtime()
                    // "speaking" stays true until the queue drains (see the idle branch).
                }
                is Item.Cue -> {
                    openI2s()
                    if (openTrack() == null) continue
                    write(Pcm.scale(item.pcm, cueGain()))
                    lastWriteAt = SystemClock.elapsedRealtime()
                }
            }
        }
        closeIdle()
    }

    companion object {
        private const val TAG = "WeftGlasses"
        /** Silence after which the track and the I2S path are closed (AudioBridge used 2 s). */
        private const val IDLE_CLOSE_MS = 1500L

        /** No write for this long after the last frame: the utterance has drained. */
        private const val DRAIN_MS = 300L
    }
}
