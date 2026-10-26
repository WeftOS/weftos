package ai.weftos.phonenode

import java.io.IOException
import java.io.InputStreamReader
import java.net.HttpURLConnection
import java.net.URL

/**
 * The HTTP the glance client needs, injected so the client is JVM-testable without sockets.
 * Implementations throw [IOException] for transport failures and return non-2xx statuses.
 */
interface CosHttp {
    class Response(val status: Int, val body: ByteArray) {
        fun text(): String = body.toString(Charsets.UTF_8)
    }

    fun get(url: String, headers: Map<String, String>): Response

    fun post(url: String, headers: Map<String, String>, body: ByteArray): Response

    /**
     * Open a long-lived GET. On a 2xx it calls [onOpen] once, then [onChunk] with decoded text
     * until the stream ends or [onChunk] returns false. Returns the HTTP status.
     */
    fun stream(url: String, headers: Map<String, String>, onOpen: () -> Unit, onChunk: (String) -> Boolean): Int

    /** Abort a [stream] in progress from another thread. */
    fun cancel()
}

/** Reconnect delay: doubling from [baseMs] to [maxMs], back to the base after a good connection. */
class Backoff(private val baseMs: Long = 1_000, private val maxMs: Long = 30_000) {
    private var next = baseMs

    fun next(): Long = next.also { next = (next * 2).coerceAtMost(maxMs) }

    fun max(): Long = maxMs.also { next = maxMs }

    fun reset() {
        next = baseMs
    }
}

/** The phone's view of the CoS link, for the status line. */
sealed interface CosLinkState {
    data object Connecting : CosLinkState

    data object Live : CosLinkState

    /** `code` is short and safe to show (`auth`, `network`, `http_503`); never the token. */
    data class Retrying(val code: String, val inMs: Long) : CosLinkState
}

sealed interface AnswerResult {
    data class Accepted(val turnId: String) : AnswerResult

    /** HTTP 400: the card was answered elsewhere, expired, or replaced. */
    data class Stale(val message: String) : AnswerResult

    data class Failed(val code: String) : AnswerResult
}

/**
 * Client for the CoS glance routes over [CosHttp]. The bearer token goes in one header and
 * nowhere else: never logged, never in a state or result. [run] holds the event stream and
 * reconnects with [Backoff]; it fetches the glance on every (re)connect, because the stream
 * has no replay.
 */
class CosGlanceClient(
    base: String,
    private val token: String,
    private val http: CosHttp,
    private val clock: () -> Long = System::currentTimeMillis,
    private val sleep: (Long) -> Unit = Thread::sleep,
    private val backoff: Backoff = Backoff(),
) {
    interface Listener {
        fun onLink(state: CosLinkState)

        fun onGlance(glance: Glance)

        fun onCard(update: CardUpdate)
    }

    private val base = base.trim().trimEnd('/')
    @Volatile private var running = false

    private fun headers(accept: String) = mapOf("Authorization" to "Bearer $token", "Accept" to accept)

    /** Null on any failure; the caller keeps what it had. */
    fun fetchGlance(): Glance? = try {
        val r = http.get("$base/v1/cos/glance", headers("application/json"))
        if (r.status in 200..299) CosGlanceWire.glance(r.text()) else null
    } catch (_: IOException) {
        null
    }

    /** The art PNG, or null when there is none (404) or the fetch failed. */
    fun fetchArt(art: GlanceArt): ByteArray? = try {
        val r = http.get(artUrl(art.pngUrl), headers("image/png"))
        if (r.status in 200..299 && r.body.isNotEmpty()) r.body else null
    } catch (_: IOException) {
        null
    }

    fun answer(cardId: String, value: String): AnswerResult {
        val body = CosGlanceWire.answerBody(cardId, value).toByteArray(Charsets.UTF_8)
        val h = headers("application/json") + ("Content-Type" to "application/json")
        val r = try {
            http.post("$base/v1/cos/answer", h, body)
        } catch (_: IOException) {
            return AnswerResult.Failed("network")
        }
        return when {
            r.status in 200..299 -> AnswerResult.Accepted(CosGlanceWire.field(r.text(), "turn_id"))
            r.status == 400 -> AnswerResult.Stale(CosGlanceWire.field(r.text(), "error"))
            else -> AnswerResult.Failed(SpeechResponse.codeForHttp(r.status))
        }
    }

    /** Hold the event stream until [stop]. Blocks the calling thread. */
    fun run(listener: Listener) {
        running = true
        val parser = SseParser()
        while (running) {
            listener.onLink(CosLinkState.Connecting)
            parser.reset()
            val status = try {
                http.stream(
                    "$base/v1/cos/events",
                    headers("text/event-stream"),
                    onOpen = {
                        backoff.reset()
                        listener.onLink(CosLinkState.Live)
                        fetchGlance()?.let(listener::onGlance)
                    },
                    onChunk = { chunk ->
                        for (ev in parser.feed(chunk)) {
                            CosGlanceWire.cardEvent(ev.data, clock())?.let(listener::onCard)
                        }
                        running
                    },
                )
            } catch (_: IOException) {
                -1
            }
            if (!running) break
            val code = when {
                status < 0 -> "network"
                status in 200..299 -> "closed"
                else -> SpeechResponse.codeForHttp(status)
            }
            // A refused token will not fix itself quickly: wait the longest step.
            val wait = if (code == "auth") backoff.max() else backoff.next()
            listener.onLink(CosLinkState.Retrying(code, wait))
            try {
                sleep(wait)
            } catch (_: InterruptedException) {
                break
            }
        }
    }

    fun stop() {
        running = false
        http.cancel()
    }

    /** The art path always hangs off the configured base, so the token never goes to another host. */
    private fun artUrl(pngUrl: String): String {
        val path = if (pngUrl.startsWith("/")) pngUrl else "/v1/cos/glance/art.png"
        return base + path
    }
}

/** [CosHttp] over [HttpURLConnection]. The stream's read timeout outlasts the 15 s keep-alive. */
class UrlConnectionCosHttp(
    private val connectTimeoutMs: Int = 5_000,
    private val readTimeoutMs: Int = 10_000,
    private val streamReadTimeoutMs: Int = 45_000,
) : CosHttp {
    @Volatile private var live: HttpURLConnection? = null

    override fun get(url: String, headers: Map<String, String>): CosHttp.Response =
        request(url, "GET", headers, null)

    override fun post(url: String, headers: Map<String, String>, body: ByteArray): CosHttp.Response =
        request(url, "POST", headers, body)

    private fun request(url: String, method: String, headers: Map<String, String>, body: ByteArray?): CosHttp.Response {
        val conn = URL(url).openConnection() as HttpURLConnection
        try {
            conn.requestMethod = method
            conn.connectTimeout = connectTimeoutMs
            conn.readTimeout = readTimeoutMs
            headers.forEach { (k, v) -> conn.setRequestProperty(k, v) }
            if (body != null) {
                conn.doOutput = true
                conn.setFixedLengthStreamingMode(body.size)
                conn.outputStream.use { it.write(body) }
            }
            val status = conn.responseCode
            val stream = if (status in 200..299) conn.inputStream else conn.errorStream
            return CosHttp.Response(status, stream?.use { it.readBytes() } ?: ByteArray(0))
        } finally {
            conn.disconnect()
        }
    }

    override fun stream(url: String, headers: Map<String, String>, onOpen: () -> Unit, onChunk: (String) -> Boolean): Int {
        val conn = URL(url).openConnection() as HttpURLConnection
        live = conn
        try {
            conn.connectTimeout = connectTimeoutMs
            conn.readTimeout = streamReadTimeoutMs
            headers.forEach { (k, v) -> conn.setRequestProperty(k, v) }
            val status = conn.responseCode
            if (status !in 200..299) return status
            onOpen()
            InputStreamReader(conn.inputStream, Charsets.UTF_8).use { r ->
                val buf = CharArray(4096)
                while (true) {
                    val n = r.read(buf)
                    if (n < 0 || !onChunk(String(buf, 0, n))) break
                }
            }
            return status
        } finally {
            live = null
            conn.disconnect()
        }
    }

    override fun cancel() {
        live?.disconnect()
    }
}
