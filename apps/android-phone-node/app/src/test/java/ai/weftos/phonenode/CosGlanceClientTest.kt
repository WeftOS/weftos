package ai.weftos.phonenode

import java.io.IOException
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class CosGlanceClientTest {
    private val base = "https://cos.example.test/"
    private val token = "tok-test-not-a-secret"

    private class Req(val method: String, val url: String, val headers: Map<String, String>, val body: ByteArray?)

    /** Scripted [CosHttp]: each stream() call plays the next script; GET/POST answer from maps. */
    private class FakeHttp : CosHttp {
        val seen = ArrayList<Req>()
        val gets = HashMap<String, CosHttp.Response>()
        var post: (ByteArray) -> CosHttp.Response = { CosHttp.Response(202, """{"turn_id":"t1"}""".toByteArray()) }
        val streams = ArrayDeque<(onOpen: () -> Unit, onChunk: (String) -> Boolean) -> Int>()
        var cancelled = 0

        override fun get(url: String, headers: Map<String, String>): CosHttp.Response {
            seen.add(Req("GET", url, headers, null))
            return gets[url] ?: throw IOException("unreachable")
        }

        override fun post(url: String, headers: Map<String, String>, body: ByteArray): CosHttp.Response {
            seen.add(Req("POST", url, headers, body))
            return post(body)
        }

        override fun stream(url: String, headers: Map<String, String>, onOpen: () -> Unit, onChunk: (String) -> Boolean): Int {
            seen.add(Req("STREAM", url, headers, null))
            val s = streams.removeFirstOrNull() ?: throw IOException("no more scripts")
            return s(onOpen, onChunk)
        }

        override fun cancel() {
            cancelled++
        }
    }

    private class Recorder : CosGlanceClient.Listener {
        val links = ArrayList<CosLinkState>()
        val glances = ArrayList<Glance>()
        val cards = ArrayList<CardUpdate>()

        override fun onLink(state: CosLinkState) {
            links.add(state)
        }

        override fun onGlance(glance: Glance) {
            glances.add(glance)
        }

        override fun onCard(update: CardUpdate) {
            cards.add(update)
        }
    }

    private val glanceBody = """{"card":null,"fields":{"state":"idle"},"art":{"caption":"c","at":"t0","png_url":"/v1/cos/glance/art.png"}}"""

    private fun client(http: FakeHttp, sleeps: MutableList<Long>, stopAfterSleeps: Int, ref: Array<CosGlanceClient?>) =
        CosGlanceClient(base, token, http, clock = { 1_000L }, sleep = { ms ->
            sleeps.add(ms)
            if (sleeps.size >= stopAfterSleeps) ref[0]!!.stop()
        }).also { ref[0] = it }

    @Test
    fun streamParsesCardsAcrossChunksAndReconnectsWithBackoffAndRefetch() {
        val http = FakeHttp()
        http.gets["https://cos.example.test/v1/cos/glance"] = CosHttp.Response(200, glanceBody.toByteArray())
        // 1st connection: a card split across chunks, a keep-alive, a non-card event, then EOF.
        http.streams.add { open, chunk ->
            open()
            chunk(": keep-alive\n\ndata: {\"kind\":\"card\",\"card\":\"now\",\"id\":\"c1\",")
            chunk("\"title\":\"Standup\",\"ttl_minutes\":1}\n\ndata: {\"kind\":\"state\",\"state\":\"idle\"}\n\n")
            chunk("data: {\"kind\":\"card\",\"card\":\"cl") // partial event lost to the drop
            200
        }
        // 2nd: connection refused. 3rd: server error. 4th: reconnects, gets the clear.
        http.streams.add { _, _ -> throw IOException("refused") }
        http.streams.add { _, _ -> 503 }
        http.streams.add { open, chunk ->
            open()
            chunk("data: {\"kind\":\"card\",\"card\":\"clear\",\"id\":\"c1\",\"reason\":\"answered\"}\n\n")
            200
        }
        val sleeps = ArrayList<Long>()
        val ref = arrayOfNulls<CosGlanceClient>(1)
        val rec = Recorder()
        client(http, sleeps, stopAfterSleeps = 4, ref).run(rec)

        assertEquals(
            listOf(
                CardUpdate.Show(CosCard(CardKind.Now, "c1", "Standup", expiresAtMs = 1_000L + 60_000)),
                CardUpdate.Clear("c1", "answered"),
            ),
            rec.cards,
        )
        // The glance is fetched on the first connect and again on the reconnect.
        assertEquals(2, rec.glances.size)
        assertEquals("c", rec.glances[0].art!!.caption)
        // Backoff doubles on failures and resets after a good connection.
        assertEquals(listOf(1_000L, 2_000L, 4_000L, 1_000L), sleeps)
        assertEquals(
            listOf(
                CosLinkState.Connecting, CosLinkState.Live, CosLinkState.Retrying("closed", 1_000),
                CosLinkState.Connecting, CosLinkState.Retrying("network", 2_000),
                CosLinkState.Connecting, CosLinkState.Retrying("server", 4_000),
                CosLinkState.Connecting, CosLinkState.Live, CosLinkState.Retrying("closed", 1_000),
            ),
            rec.links,
        )
        val stream = http.seen.first { it.method == "STREAM" }
        assertEquals("https://cos.example.test/v1/cos/events", stream.url)
        assertEquals("Bearer $token", stream.headers["Authorization"])
        assertEquals("text/event-stream", stream.headers["Accept"])
        assertTrue(http.cancelled > 0)
    }

    @Test
    fun refusedTokenWaitsTheLongestStep() {
        val http = FakeHttp()
        http.streams.add { _, _ -> 401 }
        http.streams.add { _, _ -> 403 }
        val sleeps = ArrayList<Long>()
        val ref = arrayOfNulls<CosGlanceClient>(1)
        val rec = Recorder()
        client(http, sleeps, stopAfterSleeps = 2, ref).run(rec)
        assertEquals(listOf(30_000L, 30_000L), sleeps)
        assertEquals(CosLinkState.Retrying("auth", 30_000), rec.links[1])
        assertTrue(rec.glances.isEmpty())
    }

    @Test
    fun onChunkStopsTheStreamOnceStopped() {
        val http = FakeHttp()
        val ref = arrayOfNulls<CosGlanceClient>(1)
        var chunksAfterStop = 0
        http.streams.add { open, chunk ->
            open()
            ref[0]!!.stop()
            if (chunk("data: {}\n\n")) chunksAfterStop++
            200
        }
        val sleeps = ArrayList<Long>()
        client(http, sleeps, stopAfterSleeps = 99, ref).run(Recorder())
        assertEquals(0, chunksAfterStop)
        assertTrue(sleeps.isEmpty())
    }

    @Test
    fun answerRequestShape() {
        val http = FakeHttp()
        val c = CosGlanceClient(base, token, http)
        assertEquals(AnswerResult.Accepted("t1"), c.answer("c1", "Ship"))
        val r = http.seen.single()
        assertEquals("POST", r.method)
        assertEquals("https://cos.example.test/v1/cos/answer", r.url)
        assertEquals("Bearer $token", r.headers["Authorization"])
        assertEquals("application/json", r.headers["Content-Type"])
        val j = JSONObject(String(r.body!!, Charsets.UTF_8))
        assertEquals("answer", j.getString("kind"))
        assertEquals("c1", j.getString("id"))
        assertEquals("Ship", j.getString("value"))
    }

    @Test
    fun answer400IsStaleAndOtherFailuresAreCodes() {
        val http = FakeHttp()
        val c = CosGlanceClient(base, token, http)
        http.post = { CosHttp.Response(400, """{"error":"no live card c1"}""".toByteArray()) }
        assertEquals(AnswerResult.Stale("no live card c1"), c.answer("c1", "Ship"))
        http.post = { CosHttp.Response(400, ByteArray(0)) }
        assertEquals(AnswerResult.Stale(""), c.answer("c1", "Ship"))
        http.post = { CosHttp.Response(401, """{"error":"unauthorized"}""".toByteArray()) }
        assertEquals(AnswerResult.Failed("auth"), c.answer("c1", "Ship"))
        http.post = { CosHttp.Response(500, ByteArray(0)) }
        assertEquals(AnswerResult.Failed("server"), c.answer("c1", "Ship"))
        http.post = { throw IOException("down") }
        assertEquals(AnswerResult.Failed("network"), c.answer("c1", "Ship"))
    }

    @Test
    fun artFetchStaysOnTheBaseHostAnd404IsNoArt() {
        val http = FakeHttp()
        val png = byteArrayOf(0x89.toByte(), 'P'.code.toByte(), 'N'.code.toByte(), 'G'.code.toByte())
        http.gets["https://cos.example.test/v1/cos/glance/art.png"] = CosHttp.Response(200, png)
        val c = CosGlanceClient(base, token, http)
        assertArrayEquals(png, c.fetchArt(GlanceArt("c", "t", "/v1/cos/glance/art.png")))
        // An absolute URL elsewhere is not followed with the token.
        assertArrayEquals(png, c.fetchArt(GlanceArt("c", "t", "https://other.example.test/x.png")))
        assertFalse(http.seen.any { it.url.contains("other.example.test") })
        http.gets["https://cos.example.test/v1/cos/glance/art.png"] = CosHttp.Response(404, """{"error":"no art yet"}""".toByteArray())
        assertNull(c.fetchArt(GlanceArt("c", "t", "/v1/cos/glance/art.png")))
    }

    @Test
    fun glanceFetchFailureIsNull() {
        val http = FakeHttp()
        val c = CosGlanceClient(base, token, http)
        assertNull(c.fetchGlance())
        http.gets["https://cos.example.test/v1/cos/glance"] = CosHttp.Response(500, ByteArray(0))
        assertNull(c.fetchGlance())
    }

    @Test
    fun backoffDoublesCapsAndResets() {
        val b = Backoff(1_000, 5_000)
        assertEquals(listOf(1_000L, 2_000L, 4_000L, 5_000L, 5_000L), List(5) { b.next() })
        b.reset()
        assertEquals(1_000L, b.next())
    }
}
