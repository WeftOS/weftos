package com.weftos.glasses

import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.SystemClock
import android.util.Log

/**
 * AudioRecord loop (16 kHz mono PCM16, VOICE_RECOGNITION with MIC fallback, as the proven
 * mentramic app does). Cuts the stream into 20 ms frames and hands each to [onFrame]. If the
 * mic cannot start (permission, another holder) it reports the error and retries; a silent
 * mic (all zero samples) is visible through [silentMs] so gate G0 can be read from status.
 */
class MicCapture(
    private val onFrame: (frame: ByteArray, tsMs: Long) -> Unit,
    private val onState: (String) -> Unit,
) {
    @Volatile var enabled = true
    @Volatile var level = 0
        private set
    @Volatile var peak = 0
        private set
    @Volatile var source = "none"
        private set
    @Volatile private var lastNonZeroAt = SystemClock.elapsedRealtime()
    @Volatile private var running = false
    private var thread: Thread? = null

    val silentMs: Long get() = SystemClock.elapsedRealtime() - lastNonZeroAt

    fun start() {
        if (thread != null) return
        running = true
        thread = Thread({ loop() }, "glasses-mic").also {
            it.isDaemon = true
            it.start()
        }
    }

    fun stop() {
        running = false
        thread?.interrupt()
        thread?.join(1000)
        thread = null
    }

    private fun open(sourceId: Int, bufBytes: Int): AudioRecord? = try {
        val r = AudioRecord(
            sourceId, AudioFormatP0.SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO,
            AudioFormat.ENCODING_PCM_16BIT, bufBytes,
        )
        if (r.state == AudioRecord.STATE_INITIALIZED) r else { r.release(); null }
    } catch (e: SecurityException) {
        // RECORD_AUDIO not granted: the caller treats a null source as "no mic".
        Log.w(TAG, "open source $sourceId: RECORD_AUDIO not granted", e)
        null
    } catch (e: Exception) {
        Log.w(TAG, "open source $sourceId failed", e)
        null
    }

    private fun loop() {
        while (running) {
            if (!enabled) {
                onState("off")
                sleep(250)
                continue
            }
            val min = AudioRecord.getMinBufferSize(
                AudioFormatP0.SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
            )
            val buf = maxOf(min, AudioFormatP0.SAMPLE_RATE / 4 * 2)
            var rec = open(MediaRecorder.AudioSource.VOICE_RECOGNITION, buf)
            source = "VOICE_RECOGNITION"
            if (rec == null) {
                rec = open(MediaRecorder.AudioSource.MIC, buf)
                source = "MIC"
            }
            if (rec == null) {
                onState("error: AudioRecord init failed")
                sleep(RETRY_MS)
                continue
            }
            try {
                rec.startRecording()
                if (rec.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                    onState("error: startRecording refused (mic busy or permission missing)")
                    sleep(RETRY_MS)
                    continue
                }
                onState("recording")
                readLoop(rec)
            } catch (e: Exception) {
                onState("error: ${e.message}")
                sleep(RETRY_MS)
            } finally {
                try { rec.stop() } catch (_: Exception) { }
                rec.release()
            }
        }
    }

    private fun readLoop(rec: AudioRecord) {
        val chunker = PcmChunker()
        val buf = ByteArray(AudioFormatP0.FRAME_BYTES * 2)
        while (running && enabled) {
            val n = rec.read(buf, 0, buf.size)
            if (n < 0) throw IllegalStateException("AudioRecord.read error $n")
            if (n == 0) continue
            val now = SystemClock.elapsedRealtime()
            for (frame in chunker.push(buf, 0, n)) {
                val p = Pcm.peak(frame)
                level = Pcm.rms(frame)
                peak = p
                if (p > 0) lastNonZeroAt = now
                onFrame(frame, now)
            }
        }
    }

    private fun sleep(ms: Long) {
        try { Thread.sleep(ms) } catch (_: InterruptedException) { }
    }

    companion object {
        private const val TAG = "WeftGlasses"
        private const val RETRY_MS = 3000L
    }
}
