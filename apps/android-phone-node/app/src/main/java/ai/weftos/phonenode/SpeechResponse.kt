package ai.weftos.phonenode

import org.json.JSONException
import org.json.JSONObject
import java.util.Base64

/**
 * What the speech endpoint said, reduced to the three outcomes the glasses indicator cares
 * about. Pure JVM: the Rust-facing glue (`LinkEngine`) turns a [Verdict] into a `RelayResult`.
 *
 * Response contract (`POST /api/speech/turn`):
 * `{status:"ok"|"no_speech"|"error", turn_id, transcript, reply_text, spoken_text,
 *   audio:{format:"pcm_s16le",rate:16000,channels:1,b64}, timings_ms:{...}}`
 * or `{status:"error", code, message}`.
 */
sealed interface Verdict {
    class Reply(
        val pcm: ByteArray,
        val transcript: String,
        val replyText: String,
        val spokenText: String,
        val timingsMs: Map<String, Long>,
    ) : Verdict

    data class NoSpeech(val transcript: String = "") : Verdict

    /** `code` is short and safe to show: it never carries the token or the audio. */
    data class Failed(val code: String, val message: String = "") : Verdict
}

object SpeechResponse {
    /** Map an HTTP status and body to a [Verdict]. */
    fun parse(httpStatus: Int, body: String): Verdict {
        val json = try {
            JSONObject(body)
        } catch (_: JSONException) {
            return Verdict.Failed(codeForHttp(httpStatus), "response was not JSON")
        }
        return when (json.optString("status")) {
            "ok" -> if (httpStatus in 200..299) reply(json) else Verdict.Failed(codeForHttp(httpStatus))
            "no_speech" -> Verdict.NoSpeech(json.optString("transcript"))
            "error" -> Verdict.Failed(
                json.optString("code").ifEmpty { codeForHttp(httpStatus) },
                json.optString("message"),
            )
            else -> Verdict.Failed(
                if (httpStatus in 200..299) "bad_response" else codeForHttp(httpStatus),
                "missing or unknown status",
            )
        }
    }

    fun codeForHttp(status: Int): String = when {
        status in 200..299 -> "bad_response"
        status == 401 || status == 403 -> "auth"
        status == 402 -> "budget"
        status == 413 -> "too_large"
        status == 429 -> "rate_limited"
        status >= 500 -> "server"
        else -> "http_$status"
    }

    private fun reply(json: JSONObject): Verdict {
        val audio = json.optJSONObject("audio")
        val pcm = if (audio == null || audio.optString("b64").isEmpty()) {
            ByteArray(0)
        } else {
            if (audio.optString("format") != "pcm_s16le" ||
                audio.optInt("rate") != Wav.SAMPLE_RATE ||
                audio.optInt("channels") != 1
            ) {
                return Verdict.Failed("bad_audio_format", "expected pcm_s16le 16000 Hz mono")
            }
            try {
                Base64.getDecoder().decode(audio.getString("b64"))
            } catch (_: IllegalArgumentException) {
                return Verdict.Failed("bad_audio", "reply audio was not valid base64")
            }
        }
        val timings = HashMap<String, Long>()
        json.optJSONObject("timings_ms")?.let { t ->
            for (k in t.keys()) timings[k] = t.optLong(k)
        }
        return Verdict.Reply(
            pcm = pcm,
            transcript = json.optString("transcript"),
            replyText = json.optString("reply_text"),
            spokenText = json.optString("spoken_text"),
            timingsMs = timings,
        )
    }
}
