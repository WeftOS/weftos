package ai.weftos.phonenode

import java.nio.ByteBuffer
import java.nio.ByteOrder
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class WavTest {
    private fun pcm(ms: Int) = ByteArray(ms * Wav.BYTES_PER_MS) { (it % 251).toByte() }

    @Test
    fun headerDescribes16kMonoPcm16() {
        val data = pcm(250)
        val w = Wav.pcm16Mono(data)
        assertEquals(44 + data.size, w.size)
        val b = ByteBuffer.wrap(w).order(ByteOrder.LITTLE_ENDIAN)
        assertEquals("RIFF", String(w, 0, 4, Charsets.US_ASCII))
        assertEquals(36 + data.size, b.getInt(4))
        assertEquals("WAVEfmt ", String(w, 8, 8, Charsets.US_ASCII))
        assertEquals(16, b.getInt(16))
        assertEquals(1, b.getShort(20).toInt()) // PCM
        assertEquals(1, b.getShort(22).toInt()) // mono
        assertEquals(16_000, b.getInt(24))
        assertEquals(32_000, b.getInt(28)) // byte rate
        assertEquals(2, b.getShort(32).toInt()) // block align
        assertEquals(16, b.getShort(34).toInt())
        assertEquals("data", String(w, 36, 4, Charsets.US_ASCII))
        assertEquals(data.size, b.getInt(40))
        assertArrayEquals(data, w.copyOfRange(44, w.size))
    }

    @Test
    fun emptyAudioStillMakesAValidHeader() {
        val w = Wav.pcm16Mono(ByteArray(0))
        assertEquals(44, w.size)
        assertEquals(36, ByteBuffer.wrap(w).order(ByteOrder.LITTLE_ENDIAN).getInt(4))
    }

    @Test
    fun durationIsBytesOver32PerMs() {
        assertEquals(1000, Wav.durationMs(pcm(1000)))
    }

    @Test
    fun limitsMatchTheEndpointWindow() {
        assertNotNull(Wav.limitProblem(pcm(199)))
        assertNull(Wav.limitProblem(pcm(200)))
        assertNull(Wav.limitProblem(pcm(15_000))) // 480 000 bytes + header stays under 500 KB
        assertNotNull(Wav.limitProblem(pcm(15_001)))
        assertNotNull(Wav.limitProblem(ByteArray(33_001))) // odd length
        assertEquals(480_044, Wav.pcm16Mono(pcm(15_000)).size)
    }
}
