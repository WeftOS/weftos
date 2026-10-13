package ai.weftos.phonenode

import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.util.Base64
import java.util.concurrent.atomic.AtomicReference
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SpeechClientTest {
    private class Seen(val requestLine: String, val headers: Map<String, String>, val body: ByteArray)

    private var server: ServerSocket? = null

    @After
    fun stop() {
        server?.close()
    }

    private fun readLine(ins: InputStream): String? {
        val b = ByteArrayOutputStream()
        while (true) {
            val c = ins.read()
            if (c < 0) return null
            if (c == '\n'.code) break
            if (c != '\r'.code) b.write(c)
        }
        return b.toString(Charsets.ISO_8859_1)
    }

    /** One-shot loopback HTTP server: records the request, answers after `delayMs`. */
    private fun serve(status: Int, response: String, delayMs: Long = 0, seen: AtomicReference<Seen>? = null): String {
        val ss = ServerSocket(0, 1, InetAddress.getLoopbackAddress())
        server = ss
        Thread {
            try {
                ss.accept().use { sock ->
                    val ins = sock.getInputStream()
                    val line = readLine(ins) ?: return@use
                    val headers = HashMap<String, String>()
                    while (true) {
                        val h = readLine(ins) ?: break
                        if (h.isEmpty()) break
                        val i = h.indexOf(':')
                        headers[h.substring(0, i).lowercase()] = h.substring(i + 1).trim()
                    }
                    val len = headers["content-length"]?.toInt() ?: 0
                    val body = ByteArray(len)
                    var off = 0
                    while (off < len) {
                        val n = ins.read(body, off, len - off)
                        if (n < 0) break
                        off += n
                    }
                    seen?.set(Seen(line, headers, body))
                    if (delayMs > 0) Thread.sleep(delayMs)
                    val out = response.toByteArray()
                    val head = "HTTP/1.1 $status X\r\nContent-Type: application/json\r\n" +
                        "Content-Length: ${out.size}\r\nConnection: close\r\n\r\n"
                    sock.getOutputStream().apply { write(head.toByteArray()); write(out); flush() }
                }
            } catch (_: Exception) {
                // the client gave up (timeout test) or the test closed the socket
            }
        }.also { it.isDaemon = true }.start()
        return "http://127.0.0.1:${ss.localPort}/api/speech/turn"
    }

    private val wav = Wav.pcm16Mono(ByteArray(32_000))
    private val reply = Base64.getEncoder().encodeToString(ByteArray(3_200))
    private val okBody =
        """{"status":"ok","turn_id":"t","transcript":"hi","reply_text":"yo","spoken_text":"yo",
           "audio":{"format":"pcm_s16le","rate":16000,"channels":1,"b64":"$reply"},"timings_ms":{"stt":100}}"""

    @Test
    fun postsTheWavWithTheDocumentedHeadersAndParsesTheReply() {
        val seen = AtomicReference<Seen>()
        val url = serve(200, okBody, seen = seen)
        val call = SpeechClient().turn(url, "secret-token-123", "42", wav, 5_000)
        val v = call.verdict as Verdict.Reply
        assertEquals("hi", v.transcript)
        assertEquals(3_200, v.pcm.size)
        assertEquals(200, call.httpStatus)
        val s = seen.get()
        assertEquals("POST /api/speech/turn HTTP/1.1", s.requestLine)
        assertEquals("Bearer secret-token-123", s.headers["authorization"])
        assertEquals("audio/wav", s.headers["content-type"])
        assertEquals("glasses", s.headers["x-weftos-endpoint"])
        assertEquals("42", s.headers["x-weftos-turn"])
        assertEquals(wav.toList(), s.body.toList())
    }

    @Test
    fun anErrorBodyKeepsItsCodeAndNeverLeaksTheToken() {
        val url = serve(402, """{"status":"error","code":"budget_exceeded","message":"cap"}""")
        val v = SpeechClient().turn(url, "secret-token-123", "1", wav, 5_000).verdict as Verdict.Failed
        assertEquals("budget_exceeded", v.code)
        assertFalse(v.toString().contains("secret-token-123"))
    }

    @Test
    fun unauthorizedIsAnAuthFailure() {
        val url = serve(401, """{"status":"error","code":"unauthorized","message":"bad token"}""")
        val c = SpeechClient().turn(url, "secret-token-123", "1", wav, 5_000)
        // HttpURLConnection may hide the body of a streamed 401; the status alone must map.
        assertTrue((c.verdict as Verdict.Failed).code in setOf("auth", "unauthorized"))
        assertEquals(401, c.httpStatus)
    }

    @Test
    fun aSlowServerTimesOutWithinTheBudget() {
        val url = serve(200, okBody, delayMs = 3_000)
        val t0 = System.nanoTime()
        val call = SpeechClient().turn(url, "t", "1", wav, 1_000)
        val ms = (System.nanoTime() - t0) / 1_000_000
        assertEquals(Verdict.Failed("timeout"), call.verdict)
        assertTrue("took $ms ms", ms < 2_500)
    }

    @Test
    fun aClosedPortIsANetworkFailure() {
        val url = serve(200, okBody)
        server!!.close()
        server = null
        assertEquals(Verdict.Failed("network"), SpeechClient().turn(url, "t", "1", wav, 2_000).verdict)
    }

    @Test
    fun aMalformedEndpointIsABadEndpoint() {
        val c = SpeechClient().turn("not a url", "t", "1", wav, 1_000)
        assertEquals("bad_endpoint", (c.verdict as Verdict.Failed).code)
    }
}
