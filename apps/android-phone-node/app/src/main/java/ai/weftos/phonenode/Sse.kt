package ai.weftos.phonenode

/** One dispatched Server-Sent Event. `event` is "message" when the stream names none. */
data class SseEvent(val event: String, val data: String, val id: String?)

/**
 * Incremental `text/event-stream` parser (WHATWG HTML, "Server-sent events", the parsing
 * rules). Feed it decoded text in whatever pieces the socket delivers; it keeps a partial
 * line across calls and returns the events completed by each piece. Pure JVM.
 *
 * - Lines end in LF, CRLF or a lone CR (a CR at the end of one chunk and an LF at the start
 *   of the next are one line end).
 * - Lines starting with `:` are comments (keep-alives) and are dropped.
 * - Several `data:` lines join with `\n`; a blank line dispatches; an event with no data is
 *   not dispatched.
 * - A partial event left when the stream ends is discarded, as the spec requires; [reset]
 *   does the same before a reconnect.
 */
class SseParser {
    private val line = StringBuilder()
    private val data = StringBuilder()
    private var hasData = false
    private var event = ""
    private var lastId: String? = null
    private var pendingCr = false
    private var first = true

    fun feed(chunk: String): List<SseEvent> {
        val out = ArrayList<SseEvent>()
        for (ch in chunk) {
            if (first) {
                first = false
                if (ch == '\uFEFF') continue
            }
            if (pendingCr) {
                pendingCr = false
                if (ch == '\n') continue
            }
            when (ch) {
                '\r' -> {
                    pendingCr = true
                    endLine(out)
                }
                '\n' -> endLine(out)
                else -> line.append(ch)
            }
        }
        return out
    }

    /** Drop any partial line or event (stream closed, about to reconnect). Keeps the last id. */
    fun reset() {
        line.setLength(0)
        data.setLength(0)
        hasData = false
        event = ""
        pendingCr = false
        first = true
    }

    private fun endLine(out: MutableList<SseEvent>) {
        val l = line.toString()
        line.setLength(0)
        if (l.isEmpty()) {
            dispatch(out)
            return
        }
        if (l[0] == ':') return
        val colon = l.indexOf(':')
        val field = if (colon < 0) l else l.substring(0, colon)
        var value = if (colon < 0) "" else l.substring(colon + 1)
        if (value.startsWith(' ')) value = value.substring(1)
        when (field) {
            "data" -> {
                if (hasData) data.append('\n')
                data.append(value)
                hasData = true
            }
            "event" -> event = value
            "id" -> if ('\u0000' !in value) lastId = value
            else -> {} // "retry" and unknown fields: ignored (the client owns its backoff)
        }
    }

    private fun dispatch(out: MutableList<SseEvent>) {
        if (hasData) out.add(SseEvent(event.ifEmpty { "message" }, data.toString(), lastId))
        data.setLength(0)
        hasData = false
        event = ""
    }
}
