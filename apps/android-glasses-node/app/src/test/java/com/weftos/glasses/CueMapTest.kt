package com.weftos.glasses

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.weftos_node_core.Cue
import uniffi.weftos_node_core.CueKind

class CueMapTest {
    @Test
    fun indicatorCuesMapToTheirAssets() {
        assertEquals(CueAsset.LISTENING_START, CueMap.forIndicator(Cue.LISTENING_START))
        assertEquals(CueAsset.CAPTURED, CueMap.forIndicator(Cue.CAPTURED))
        assertEquals(CueAsset.DONE_OK, CueMap.forIndicator(Cue.DONE_OK))
        assertEquals(CueAsset.DONE_ERROR, CueMap.forIndicator(Cue.DONE_ERROR))
    }

    @Test
    fun protocolCueKindsMapToTheSameVoice() {
        assertEquals(CueAsset.LISTENING_START, CueMap.forKind(CueKind.HEARD))
        assertEquals(CueAsset.THINKING_TICK, CueMap.forKind(CueKind.WORKING))
        assertEquals(CueAsset.DONE_OK, CueMap.forKind(CueKind.DONE))
        assertEquals(CueAsset.DONE_ERROR, CueMap.forKind(CueKind.ERROR))
    }

    @Test
    fun everyIndicatorCueAndEveryKindHasAnAsset() {
        for (c in Cue.values()) assertNotNull(CueMap.forIndicator(c))
        for (k in CueKind.values()) assertNotNull(CueMap.forKind(k))
    }

    @Test
    fun shellNamesResolve() {
        assertEquals(CueAsset.LISTENING_START, CueAsset.byName("listening"))
        assertEquals(CueAsset.THINKING_TICK, CueAsset.byName("Working"))
        assertEquals(CueAsset.DONE_ERROR, CueAsset.byName("error"))
        assertNull(CueAsset.byName("nope"))
    }

    /** The committed WAVs: right format, short and quiet (design 3.7: <= 250 ms, <= -16 dBFS). */
    @Test
    fun bundledAssetsAreShortQuietMono16k() {
        val assets = File("src/main/assets")
        for (a in CueAsset.values()) {
            val f = File(assets, a.file)
            assertTrue("missing ${f.path}", f.exists())
            val wav = Wav.parse(f.readBytes())
            assertEquals(a.name, 16000, wav.sampleRate)
            val ms = wav.data.size / 2 * 1000 / wav.sampleRate
            assertTrue("${a.name} runs ${ms}ms", ms in 40..250)
            val peak = Pcm.peak(wav.data)
            assertTrue("${a.name} peak $peak", peak in 1..(0.16 * 32767).toInt())
        }
    }

    @Test
    fun thinkingTickIsQuieterThanTheListeningChirp() {
        val assets = File("src/main/assets")
        fun peak(a: CueAsset) = Pcm.peak(Wav.parse(File(assets, a.file).readBytes()).data)
        assertTrue(peak(CueAsset.THINKING_TICK) < peak(CueAsset.LISTENING_START))
    }
}
