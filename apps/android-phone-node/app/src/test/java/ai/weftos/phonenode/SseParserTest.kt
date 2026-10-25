package ai.weftos.phonenode

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SseParserTest {
    @Test
    fun oneDataLineIsOneEvent() {
        val evs = SseParser().feed("data: {\"kind\":\"state\"}\n\n")
        assertEquals(listOf(SseEvent("message", "{\"kind\":\"state\"}", null)), evs)
    }

    @Test
    fun multiLineDataJoinsWithNewline() {
        val evs = SseParser().feed("data: first\ndata:second\ndata:  third\n\n")
        assertEquals("first\nsecond\n third", evs.single().data)
    }

    @Test
    fun commentsAreKeepAlivesAndDispatchNothing() {
        val p = SseParser()
        assertTrue(p.feed(":\n\n: keep-alive\n\n").isEmpty())
        assertEquals("x", p.feed(": ping\ndata: x\n\n").single().data)
    }

    @Test
    fun partialChunksAreReassembled() {
        val p = SseParser()
        assertTrue(p.feed("da").isEmpty())
        assertTrue(p.feed("ta: {\"a\":").isEmpty())
        assertTrue(p.feed("1}\n").isEmpty())
        assertEquals("{\"a\":1}", p.feed("\n").single().data)
    }

    @Test
    fun crlfAndLoneCrEndLinesIncludingAcrossChunks() {
        val p = SseParser()
        assertEquals("a", p.feed("data: a\r\n\r\n").single().data)
        assertEquals("b", p.feed("data: b\r\r").single().data)
        // CR at the end of one chunk, LF at the start of the next: one line end, not two.
        assertTrue(p.feed("data: c\r").isEmpty())
        assertTrue(p.feed("\ndata: d\r").isEmpty())
        assertEquals("c\nd", p.feed("\n\n").single().data)
    }

    @Test
    fun eventNameIdAndFieldWithoutColon() {
        val p = SseParser()
        val ev = p.feed("event: card\nid: 7\nretry: 100\nunknown\ndata\n\n").single()
        assertEquals(SseEvent("card", "", "7"), ev)
        // The event name resets after dispatch; the last id persists.
        assertEquals(SseEvent("message", "y", "7"), p.feed("data: y\n\n").single())
    }

    @Test
    fun blankLineWithoutDataDispatchesNothing() {
        assertTrue(SseParser().feed("event: x\n\n\n").isEmpty())
    }

    @Test
    fun leadingBomIsSkipped() {
        assertEquals("z", SseParser().feed("\uFEFFdata: z\n\n").single().data)
    }

    @Test
    fun resetDropsAPartialEventBeforeAReconnect() {
        val p = SseParser()
        assertTrue(p.feed("data: half of an ev").isEmpty())
        p.reset()
        assertEquals("whole", p.feed("data: whole\n\n").single().data)
    }

    @Test
    fun severalEventsInOneChunk() {
        val evs = SseParser().feed("data: 1\n\ndata: 2\n\n: k\n\ndata: 3\n\n")
        assertEquals(listOf("1", "2", "3"), evs.map { it.data })
    }
}
