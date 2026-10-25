package ai.weftos.phonenode

import java.time.Instant
import java.time.format.DateTimeParseException
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/**
 * The chief-of-staff glance card and art (ADR-118 `cos.v1`, the additive `card` event and the
 * `/v1/cos/glance` routes). Pure JVM: parsing and request shaping only, no I/O.
 *
 * Wire shapes, as the CoS serves them:
 * - event: `{"kind":"card","card":"needs_you"|"now"|"waiting_on"|"eod"|"clear", "id", "title",
 *   "detail", "alert", "options":[..], "ttl_minutes", "reason"}`; empty fields are omitted.
 * - `GET /v1/cos/glance`: `{"card": {id, kind, title, detail, alert, options, reason,
 *   expires_at}|null, "fields": {state, task, next, alert, model, ts}, "art": {caption, at,
 *   png_url}|null}`.
 * - `POST /v1/cos/answer`: `{"kind":"answer","id":"<card id>","value":"<option text>"}`; a
 *   card with no options takes free text up to [MAX_REPLY] characters. 400 leaves the card up.
 */
enum class CardKind(val wire: String, val label: String) {
    NeedsYou("needs_you", "Needs you"),
    Now("now", "Now"),
    WaitingOn("waiting_on", "Waiting on"),
    Eod("eod", "End of day"),
    ;

    companion object {
        fun parse(s: String): CardKind? = entries.firstOrNull { it.wire == s.trim().lowercase() }
    }
}

data class CosCard(
    val kind: CardKind,
    val id: String,
    val title: String,
    val detail: String = "",
    val alert: String = "",
    val options: List<String> = emptyList(),
    val reason: String = "",
    /** Wall-clock expiry in epoch ms; 0 when the CoS named none. */
    val expiresAtMs: Long = 0,
) {
    fun isLive(nowMs: Long): Boolean = expiresAtMs == 0L || nowMs < expiresAtMs
}

/** What one `card` event does to the phone's copy of the card. */
sealed interface CardUpdate {
    data class Show(val card: CosCard) : CardUpdate

    data class Clear(val id: String, val reason: String) : CardUpdate
}

data class GlanceFields(
    val state: String = "",
    val task: String = "",
    val next: String = "",
    val alert: String = "",
    val model: String = "",
    val ts: String = "",
)

data class GlanceArt(val caption: String, val at: String, val pngUrl: String)

data class Glance(val card: CosCard?, val fields: GlanceFields, val art: GlanceArt?)

object CosGlanceWire {
    /** The card event's lifetime is at most a day longer than the longest kind (EOD, 12 h). */
    private const val MAX_TTL_MINUTES = 36 * 60L
    const val MAX_OPTIONS = 2
    /** A card with no options takes free text up to this many characters. */
    const val MAX_REPLY = 200

    /** The card shown after `update`. A clear removes only the card whose id it names. */
    fun reduce(current: CosCard?, update: CardUpdate): CosCard? = when (update) {
        is CardUpdate.Show -> update.card
        is CardUpdate.Clear -> if (current != null && update.id == current.id) null else current
    }

    /** Null when `value` can be sent as an answer, otherwise what to tell the owner. */
    fun replyProblem(value: String): String? {
        val n = value.trim().codePointCount(0, value.trim().length)
        return when {
            n == 0 -> "Type a reply first."
            n > MAX_REPLY -> "Replies are at most $MAX_REPLY characters ($n now)."
            else -> null
        }
    }

    /**
     * Parse one SSE `data` payload. Null for any other event kind, a card kind this build does
     * not know, or a malformed payload: the contract says a client ignores what it cannot use.
     */
    fun cardEvent(data: String, nowMs: Long): CardUpdate? {
        val json = obj(data) ?: return null
        if (json.optString("kind") != "card") return null
        val kind = json.optString("card")
        if (kind == "clear") return CardUpdate.Clear(json.optString("id"), json.optString("reason"))
        val k = CardKind.parse(kind) ?: return null
        val ttl = json.optLong("ttl_minutes", 0).coerceIn(0, MAX_TTL_MINUTES)
        val expires = when {
            ttl > 0 -> nowMs + ttl * 60_000
            else -> instantMs(json.optString("expires_at").ifEmpty { json.optString("expires") })
        }
        return CardUpdate.Show(card(json, k, expires) ?: return null)
    }

    /** Parse the `GET /v1/cos/glance` body; null when it is not the expected object. */
    fun glance(body: String): Glance? {
        val json = obj(body) ?: return null
        val card = json.optJSONObject("card")?.let { c ->
            val k = CardKind.parse(c.optString("kind").ifEmpty { c.optString("card") }) ?: return@let null
            card(c, k, instantMs(c.optString("expires_at").ifEmpty { c.optString("expires") }))
        }
        val f = json.optJSONObject("fields")
        val fields = if (f == null) GlanceFields() else GlanceFields(
            state = str(f, "state"),
            task = str(f, "task"),
            next = str(f, "next"),
            alert = str(f, "alert"),
            model = str(f, "model"),
            ts = str(f, "ts"),
        )
        val art = json.optJSONObject("art")?.let { a ->
            GlanceArt(str(a, "caption"), str(a, "at"), str(a, "png_url").ifEmpty { "/v1/cos/glance/art.png" })
        }
        return Glance(card, fields, art)
    }

    /** The answer body. `value` is the option text, which the CoS matches case-insensitively. */
    fun answerBody(cardId: String, value: String): String =
        JSONObject().put("kind", "answer").put("id", cardId).put("value", value.trim()).toString()

    /** The `turn_id` of a 202, or the `error` text of a refusal; empty when absent. */
    fun field(body: String, name: String): String = obj(body)?.let { str(it, name) }.orEmpty()

    private fun card(json: JSONObject, kind: CardKind, expiresAtMs: Long): CosCard? {
        val id = json.optString("id")
        if (id.isEmpty()) return null // an answer needs the id; a card without one cannot be answered
        return CosCard(
            kind = kind,
            id = id,
            title = str(json, "title"),
            detail = str(json, "detail"),
            alert = str(json, "alert"),
            options = options(json.optJSONArray("options")),
            reason = str(json, "reason"),
            expiresAtMs = expiresAtMs,
        )
    }

    private fun options(a: JSONArray?): List<String> {
        if (a == null) return emptyList()
        return (0 until a.length()).mapNotNull { i -> a.optString(i).trim().takeIf { it.isNotEmpty() } }
            .take(MAX_OPTIONS)
    }

    /** A string field, treating JSON null and non-strings as empty. */
    private fun str(json: JSONObject, name: String): String =
        if (json.isNull(name)) "" else (json.opt(name) as? String).orEmpty()

    private fun instantMs(s: String): Long = if (s.isEmpty()) 0 else try {
        Instant.parse(s).toEpochMilli()
    } catch (_: DateTimeParseException) {
        0
    }

    private fun obj(s: String): JSONObject? = try {
        JSONObject(s)
    } catch (_: JSONException) {
        null
    }
}
