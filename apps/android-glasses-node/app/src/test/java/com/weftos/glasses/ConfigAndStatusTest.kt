package com.weftos.glasses

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConfigAndStatusTest {
    @Test
    fun defaultsBakeInNoAddress() {
        val c = GlassesConfig()
        assertEquals("", c.parentHost)
        assertEquals(0.4f, c.cueVolume, 0f)
        assertEquals(LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS, c.ledOrder)
        assertEquals("/dev/ttyS1", c.serialPath)
    }

    @Test
    fun parsesAndClamps() {
        val c = GlassesConfig.fromMap(
            mapOf(
                "parent_host" to " 10.0.0.2 ", "parent_port" to "99999", "cue_volume" to "3",
                "led_order" to "b", "capture_on_start" to "no", "gate_open_rms" to "900",
                "audio_usage" to "MEDIA",
            ),
        )
        assertEquals("10.0.0.2", c.parentHost)
        assertEquals(GlassesConfig.DEFAULT_PORT, c.parentPort) // out of range falls back
        assertEquals(1f, c.cueVolume, 0f)
        assertEquals(LedFieldOrder.COLOR_BRIGHTNESS_ON_OFF_CYCLES, c.ledOrder)
        assertFalse(c.captureOnStart)
        assertEquals(900, c.gate.openRms)
        assertEquals("media", c.audioUsage)
    }

    @Test
    fun overlayKeepsUntouchedValuesAndRoundTrips() {
        val base = GlassesConfig(parentHost = "h", ledOrder = LedFieldOrder.COLOR_BRIGHTNESS_ON_OFF_CYCLES)
        val c = GlassesConfig.fromMap(mapOf("cue_volume" to "0.2"), base)
        assertEquals("h", c.parentHost)
        assertEquals(LedFieldOrder.COLOR_BRIGHTNESS_ON_OFF_CYCLES, c.ledOrder)
        assertEquals(c, GlassesConfig.fromMap(c.toMap()))
    }

    @Test
    fun emptyHostClearsTheConfiguredOne() {
        val c = GlassesConfig.fromMap(mapOf("parent_host" to ""), GlassesConfig(parentHost = "h"))
        assertEquals("", c.parentHost)
    }

    private fun snap(error: String? = null) = StatusSnapshot(
        version = "0.1.0-p0", uptimeS = 12, link = "up", parent = "p:1", indicator = "idle", pairing = "idle",
        micState = "recording", micRms = 123, micPeak = 456, micSilentMs = 0, gateOpen = false, speaking = false,
        led = if (error == null) "ok" else "unavailable", ledError = error, ledLastLine = null,
        framesUp = 5, framesDown = 6, reconnects = 1, firstFrameToWriteMs = -1, nowEpochMs = 1L,
    )

    @Test
    fun statusJsonCarriesTheGateEvidenceFields() {
        val j = snap().toJson()
        for (k in listOf("link", "indicator", "mic_rms", "mic_silent_ms", "led", "frames_up")) {
            assertTrue("missing $k in $j", j.contains("\"$k\":"))
        }
        assertTrue(j.contains("\"led\":\"ok\""))
        assertTrue(j.contains("\"led_error\":null"))
    }

    @Test
    fun ledFailureShowsAsUnavailableWithTheReasonEscaped() {
        val j = snap("open \"/dev/ttyS1\": EACCES\n").toJson()
        assertTrue(j.contains("\"led\":\"unavailable\""))
        assertTrue(j.contains("open \\\"/dev/ttyS1\\\": EACCES\\n"))
        assertTrue(snap("x").logLine().contains("led=unavailable"))
    }
}
