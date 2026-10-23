package com.weftos.glasses

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EnergyGateTest {
    private fun frame(amp: Int): ByteArray {
        val b = ByteArray(AudioFormatP0.FRAME_BYTES)
        for (i in 0 until AudioFormatP0.FRAME_SAMPLES) {
            val v = if (i % 2 == 0) amp else -amp
            b[2 * i] = v.toByte()
            b[2 * i + 1] = (v shr 8).toByte()
        }
        return b
    }

    private val quiet = frame(30)
    private val loud = frame(2000)

    private fun gate(cfg: GateConfig = GateConfig()) = EnergyGate(cfg)

    @Test
    fun quietAudioNeverOpens() {
        val g = gate()
        repeat(200) { assertFalse(g.process(quiet).open) }
    }

    @Test
    fun opensOnTwoLoudFramesAndSendsTheLastQuietPreroll() {
        val g = gate()
        repeat(40) { g.process(quiet) }
        assertFalse(g.process(loud).open) // one loud frame is a click, not speech
        val r = g.process(loud)
        assertTrue(r.opened)
        // preroll (400 ms = 20 frames, one of which is the first loud frame) + current
        assertEquals(20 + 1, r.send.size)
        assertEquals(Pcm.rms(loud), Pcm.rms(r.send.last()))
    }

    @Test
    fun staysOpenThroughTheHangoverThenCloses() {
        val g = gate()
        g.process(loud); g.process(loud)
        assertTrue(g.isOpen)
        val hang = GateConfig().hangoverMs / AudioFormatP0.FRAME_MS // 40
        repeat(hang) { assertTrue(g.process(quiet).open) }
        val r = g.process(quiet)
        assertTrue(r.closed)
        assertFalse(g.isOpen)
        assertEquals(1, r.send.size) // the closing frame still goes out
    }

    @Test
    fun speechDuringHangoverResetsIt() {
        val g = gate()
        g.process(loud); g.process(loud)
        repeat(30) { g.process(quiet) }
        g.process(loud)
        repeat(30) { assertTrue(g.process(quiet).open) }
    }

    @Test
    fun speechDoesNotRaiseTheNoiseFloor() {
        val g = gate()
        repeat(50) { g.process(quiet) }
        val floor = g.noiseFloor
        assertTrue(floor in 20f..40f)
        repeat(100) { g.process(loud) }
        assertEquals(floor, g.noiseFloor, 0.001f)
    }

    @Test
    fun aNoisyRoomRaisesTheThresholdAboveTheAbsoluteFloor() {
        val g = gate()
        val noisy = frame(300) // above closeRms but below openRms 400: still no open
        repeat(100) { assertFalse(g.process(noisy).open) }
        assertTrue(g.noiseFloor > 250f)
        // 2x noise (600) clears the absolute 400 but not 3x the 300 floor (900): stays shut
        repeat(10) { assertFalse(g.process(frame(600)).open) }
        repeat(2) { g.process(frame(1500)) }
        assertTrue(g.isOpen)
    }

    @Test
    fun forceHoldsTheGateOpenAndOpensImmediately() {
        val g = gate()
        g.force(true)
        val r = g.process(quiet)
        assertTrue(r.opened && r.open)
        repeat(100) { assertTrue(g.process(quiet).open) }
        g.force(false)
        repeat(GateConfig().hangoverMs / AudioFormatP0.FRAME_MS + 1) { g.process(quiet) }
        assertFalse(g.isOpen)
    }

    @Test
    fun maxOpenForcesAClose() {
        val g = gate(GateConfig(maxOpenMs = 1000, prerollMs = 0))
        g.process(loud); g.process(loud)
        var closed = false
        repeat(60) { if (g.process(loud).closed) closed = true }
        assertTrue(closed)
    }

    @Test
    fun resetDropsPrerollAndState() {
        val g = gate()
        g.process(loud); g.process(loud)
        g.reset()
        assertFalse(g.isOpen)
        repeat(1) { assertFalse(g.process(loud).open) } // needs two loud frames again
    }
}
