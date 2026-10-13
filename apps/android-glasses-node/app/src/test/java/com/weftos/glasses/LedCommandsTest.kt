package com.weftos.glasses

import java.io.IOException
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.weftos_node_core.LedColor
import uniffi.weftos_node_core.LedPattern

class LedCommandsTest {
    private fun pat(c: LedColor, on: UInt, off: UInt, cycles: UInt, b: UInt) =
        LedPattern(color = c, onMs = on, offMs = off, cycles = cycles, brightness = b.toUByte())

    @Test
    fun rendersTheDocumentedGreenExample() {
        // Design 3.6 example: green, 3100 ms on, 100 off, 1 cycle, brightness 180. Same string
        // as the Rust test `renders_documented_example`.
        val p = pat(LedColor.GREEN, 3100u, 100u, 1u, 180u)
        assertEquals(
            "{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"1,3100,100,1,180\"}\n",
            LedCommands.set(p, LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS),
        )
    }

    @Test
    fun fieldOrderIsConfigurable() {
        val p = pat(LedColor.GREEN, 500u, 400u, 3u, 200u)
        // Same strings as the Rust test `field_order_is_selectable`.
        assertEquals(
            "{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"1,500,400,3,200\"}\n",
            LedCommands.set(p, LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS),
        )
        assertEquals(
            "{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"1,200,500,400,3\"}\n",
            LedCommands.set(p, LedFieldOrder.COLOR_BRIGHTNESS_ON_OFF_CYCLES),
        )
    }

    @Test
    fun colourCodesAreRedGreenBlueOrangeWhite() {
        assertEquals(listOf(0, 1, 2, 3, 4), LedColor.values().map { LedCommands.colorCode(it) })
        assertEquals(LedColor.RED, LedColor.values()[0])
    }

    @Test
    fun claimReleaseOffAndI2sLines() {
        assertEquals("{\"C\":\"android_control_led\",\"V\":1,\"B\":\"1\"}\n", LedCommands.claim())
        assertEquals("{\"C\":\"android_control_led\",\"V\":1,\"B\":\"0\"}\n", LedCommands.release())
        assertEquals("{\"C\":\"cs_ledoff\",\"V\":1,\"B\":\"\"}\n", LedCommands.off())
        assertEquals("{\"C\":\"mh_starti2s\",\"V\":1,\"B\":\"\"}\n", LedCommands.i2sStart())
        assertEquals("{\"C\":\"mh_stopi2s\",\"V\":1,\"B\":\"\"}\n", LedCommands.i2sStop())
    }

    @Test
    fun everyLineIsOneWholeLine() {
        val lines = listOf(
            LedCommands.claim(), LedCommands.release(), LedCommands.off(),
            LedCommands.set(pat(LedColor.BLUE, 60000u, 0u, 1u, 160u), LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS),
        )
        for (l in lines) {
            assertTrue(l.endsWith("\n"))
            assertEquals(1, l.count { it == '\n' })
            assertTrue(l.startsWith("{") && l.trimEnd().endsWith("}"))
        }
    }
}

class LedWriterTest {
    private class Recording : LineSink {
        val lines = ArrayList<String>()
        @Volatile var fail = false
        override fun writeLine(line: String) {
            if (fail) throw IOException("EACCES")
            synchronized(lines) { lines.add(line) }
        }
    }

    private fun writer(sink: Recording): Pair<LedWriter, SerialQueue> {
        lateinit var w: LedWriter
        val q = SerialQueue(sink) { r -> w.onSerialResult(r) }
        w = LedWriter(q)
        return w to q
    }

    private val blue = LedPattern(LedColor.BLUE, 60000u, 0u, 1u, 160u.toUByte())
    private val orange = LedPattern(LedColor.ORANGE, 900u, 900u, 34u, 90u.toUByte())

    @Test
    fun claimsOnceThenSetsThenOffAndReleases() {
        val sink = Recording()
        val (w, q) = writer(sink)
        w.show(blue)
        w.show(orange)
        w.release()
        w.release() // already released: nothing more is written
        assertTrue(q.await(2000))
        assertEquals(
            listOf(
                LedCommands.claim(),
                LedCommands.set(blue, LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS),
                LedCommands.set(orange, LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS),
                LedCommands.off(),
                LedCommands.release(),
            ),
            sink.lines,
        )
        assertTrue(w.available)
    }

    @Test
    fun aFailedWriteMarksTheLedUnavailableAndTheNextOneRecovers() {
        val sink = Recording().also { it.fail = true }
        val (w, q) = writer(sink)
        w.show(blue)
        assertTrue(q.await(2000))
        assertFalse(w.available)
        assertEquals("EACCES", w.lastError)
        sink.fail = false
        w.show(orange)
        assertTrue(q.await(2000))
        assertTrue(w.available)
    }

    @Test
    fun disabledWriterWritesNothing() {
        val sink = Recording()
        val (w, q) = writer(sink)
        w.enabled = false
        w.show(blue)
        w.release()
        assertTrue(q.await(2000))
        assertTrue(sink.lines.isEmpty())
    }

    @Test
    fun fieldOrderChangeAppliesToTheNextSet() {
        val sink = Recording()
        val (w, q) = writer(sink)
        w.order = LedFieldOrder.COLOR_BRIGHTNESS_ON_OFF_CYCLES
        w.show(orange)
        assertTrue(q.await(2000))
        assertEquals("{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"3,90,900,900,34\"}\n", sink.lines.last())
    }
}
