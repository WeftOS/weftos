package com.weftos.glasses

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AudioChunkerTest {
    private fun bytes(n: Int, start: Int = 0) = ByteArray(n) { (start + it).toByte() }

    @Test
    fun twentyMsAt16kHzIs640Bytes() {
        assertEquals(320, AudioFormatP0.FRAME_SAMPLES)
        assertEquals(640, AudioFormatP0.FRAME_BYTES)
    }

    @Test
    fun exactMultiplesBecomeWholeFrames() {
        val c = PcmChunker()
        val frames = c.push(bytes(1920))
        assertEquals(3, frames.size)
        assertTrue(frames.all { it.size == 640 })
        assertEquals(0, c.pending)
    }

    @Test
    fun remaindersCarryIntoTheNextPush() {
        val c = PcmChunker()
        val data = bytes(1000)
        assertEquals(1, c.push(data, 0, 700).size)
        assertEquals(60, c.pending)
        val next = c.push(data, 700, 300) // 60 carried + 300 = 360 < 640
        assertTrue(next.isEmpty())
        assertEquals(360, c.pending)
        val more = c.push(bytes(300, 7)) // 660 total -> one frame, 20 left
        assertEquals(1, more.size)
        assertEquals(20, c.pending)
    }

    @Test
    fun chunkingIsLosslessAndOrdered() {
        val data = bytes(4000)
        val c = PcmChunker()
        val frames = ArrayList<ByteArray>()
        var off = 0
        for (n in listOf(1, 639, 700, 1, 1500, 1159)) { // awkward AudioRecord read sizes
            frames.addAll(c.push(data, off, n))
            off += n
        }
        assertEquals(4000, off)
        val joined = frames.fold(ByteArray(0)) { a, b -> a + b }
        assertArrayEquals(data.copyOf(joined.size), joined)
        assertEquals(4000 / 640, frames.size)
        assertEquals(4000 % 640, c.pending)
    }

    @Test
    fun splitPaddedKeepsTheTailWithSilence() {
        val frames = PcmChunker.splitPadded(bytes(700, 1))
        assertEquals(2, frames.size)
        assertEquals((1 + 640).toByte(), frames[1][0]) // byte 640 of the input
        assertEquals(0.toByte(), frames[1][100]) // padded
    }

    @Test(expected = IllegalArgumentException::class)
    fun oddFrameSizesAreRejected() {
        PcmChunker(641)
    }

    @Test
    fun rmsAndPeakAndScale() {
        val loud = ByteArray(640)
        for (i in 0 until 320) { // +/-1000 square wave
            val v = if (i % 2 == 0) 1000 else -1000
            loud[2 * i] = v.toByte()
            loud[2 * i + 1] = (v shr 8).toByte()
        }
        assertEquals(1000, Pcm.rms(loud))
        assertEquals(1000, Pcm.peak(loud))
        assertEquals(500, Pcm.peak(Pcm.scale(loud, 0.5f)))
        assertEquals(0, Pcm.peak(Pcm.scale(loud, 0f)))
        assertEquals(32768, Pcm.peak(Pcm.scale(loud, 100f))) // clipped at the rail, not wrapped
    }
}
