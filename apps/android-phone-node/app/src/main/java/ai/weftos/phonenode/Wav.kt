package ai.weftos.phonenode

import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The speech endpoint takes a 16 kHz mono PCM16 WAV of 0.2 to 15 s, at most 500 KB.
 * The Rust core already enforces the length window when it assembles an utterance; the
 * checks here keep the HTTP layer honest on its own.
 */
object Wav {
    const val SAMPLE_RATE = 16_000
    const val BYTES_PER_MS = 32
    const val HEADER_BYTES = 44
    const val MIN_MS = 200
    const val MAX_MS = 15_000
    const val MAX_BYTES = 500_000

    fun durationMs(pcm: ByteArray): Int = pcm.size / BYTES_PER_MS

    /** Null when `pcm` fits the endpoint's window, otherwise why it does not. */
    fun limitProblem(pcm: ByteArray): String? = when {
        pcm.size % 2 != 0 -> "odd PCM16 length"
        durationMs(pcm) < MIN_MS -> "shorter than ${MIN_MS} ms"
        durationMs(pcm) > MAX_MS -> "longer than ${MAX_MS} ms"
        pcm.size + HEADER_BYTES > MAX_BYTES -> "larger than ${MAX_BYTES} bytes"
        else -> null
    }

    /** Wrap raw little-endian PCM16 mono samples in a canonical 44-byte RIFF header. */
    fun pcm16Mono(pcm: ByteArray): ByteArray {
        val out = ByteArrayOutputStream(HEADER_BYTES + pcm.size)
        val h = ByteBuffer.allocate(HEADER_BYTES).order(ByteOrder.LITTLE_ENDIAN)
        h.put("RIFF".toByteArray(Charsets.US_ASCII))
        h.putInt(36 + pcm.size)
        h.put("WAVE".toByteArray(Charsets.US_ASCII))
        h.put("fmt ".toByteArray(Charsets.US_ASCII))
        h.putInt(16) // fmt chunk size
        h.putShort(1) // PCM
        h.putShort(1) // mono
        h.putInt(SAMPLE_RATE)
        h.putInt(SAMPLE_RATE * 2) // byte rate
        h.putShort(2) // block align
        h.putShort(16) // bits per sample
        h.put("data".toByteArray(Charsets.US_ASCII))
        h.putInt(pcm.size)
        out.write(h.array())
        out.write(pcm)
        return out.toByteArray()
    }
}
