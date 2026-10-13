package com.weftos.glasses

/** Wire audio format of P0: 16 kHz mono PCM16, 20 ms frames (design 4.2). */
object AudioFormatP0 {
    const val SAMPLE_RATE = 16_000
    const val FRAME_MS = 20
    const val FRAME_SAMPLES = SAMPLE_RATE * FRAME_MS / 1000 // 320
    const val FRAME_BYTES = FRAME_SAMPLES * 2 // 640
}

/**
 * Cuts a PCM16 byte stream into fixed-size frames. `AudioRecord.read` and WAV assets
 * return arbitrary lengths; the wire wants exactly [frameBytes] per frame, so the tail
 * of each push carries over to the next one.
 */
class PcmChunker(private val frameBytes: Int = AudioFormatP0.FRAME_BYTES) {
    private var carry = ByteArray(0)

    init {
        require(frameBytes > 0 && frameBytes % 2 == 0) { "frame size must be a whole number of samples" }
    }

    /** Bytes held back waiting for the rest of a frame. */
    val pending: Int get() = carry.size

    fun push(data: ByteArray, offset: Int = 0, length: Int = data.size - offset): List<ByteArray> {
        require(offset >= 0 && length >= 0 && offset + length <= data.size) { "bad range" }
        val all = if (carry.isEmpty()) data.copyOfRange(offset, offset + length)
        else carry + data.copyOfRange(offset, offset + length)
        val full = all.size / frameBytes
        val out = ArrayList<ByteArray>(full)
        for (i in 0 until full) out.add(all.copyOfRange(i * frameBytes, (i + 1) * frameBytes))
        carry = all.copyOfRange(full * frameBytes, all.size)
        return out
    }

    /** Drop the partial tail (end of a stream; a partial frame is never sent). */
    fun reset() {
        carry = ByteArray(0)
    }

    companion object {
        /** Split a whole buffer; the partial tail is padded with silence to a full frame. */
        fun splitPadded(pcm: ByteArray, frameBytes: Int = AudioFormatP0.FRAME_BYTES): List<ByteArray> {
            val c = PcmChunker(frameBytes)
            val frames = ArrayList(c.push(pcm))
            if (c.pending > 0) {
                val tail = ByteArray(frameBytes)
                val rest = pcm.copyOfRange(pcm.size - c.pending, pcm.size)
                System.arraycopy(rest, 0, tail, 0, rest.size)
                frames.add(tail)
            }
            return frames
        }
    }
}

/** PCM16 little-endian helpers. */
object Pcm {
    fun rms(frame: ByteArray): Int {
        val n = frame.size / 2
        if (n == 0) return 0
        var sum = 0.0
        for (i in 0 until n) {
            val s = ((frame[2 * i].toInt() and 0xff) or (frame[2 * i + 1].toInt() shl 8)).toShort().toInt()
            sum += s.toDouble() * s
        }
        return Math.sqrt(sum / n).toInt()
    }

    fun peak(frame: ByteArray): Int {
        var p = 0
        for (i in 0 until frame.size / 2) {
            val s = ((frame[2 * i].toInt() and 0xff) or (frame[2 * i + 1].toInt() shl 8)).toShort().toInt()
            p = maxOf(p, Math.abs(s))
        }
        return p
    }

    /** Scale in place-safe fashion: returns a new buffer, clipped. */
    fun scale(frame: ByteArray, gain: Float): ByteArray {
        if (gain == 1.0f) return frame
        val out = ByteArray(frame.size)
        for (i in 0 until frame.size / 2) {
            val s = ((frame[2 * i].toInt() and 0xff) or (frame[2 * i + 1].toInt() shl 8)).toShort().toInt()
            val v = (s * gain).toInt().coerceIn(-32768, 32767)
            out[2 * i] = v.toByte()
            out[2 * i + 1] = (v shr 8).toByte()
        }
        return out
    }
}
