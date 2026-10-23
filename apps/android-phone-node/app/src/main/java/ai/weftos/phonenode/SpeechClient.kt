package ai.weftos.phonenode

import java.io.IOException
import java.net.HttpURLConnection
import java.net.SocketTimeoutException
import java.net.URL

/**
 * Blocking HTTP client for the cloud speech endpoint. Pure JVM so it is testable against a
 * local server. The token goes in one header and nowhere else: it is never logged, never put
 * in an exception message and never part of a [Verdict].
 */
class SpeechClient(private val connectTimeoutMs: Int = 5_000) {
    class Call(val verdict: Verdict, val httpStatus: Int, val rttMs: Long)

    /**
     * POST one utterance. `budgetMs` bounds the read: the design gives the whole turn 20 s.
     * Never throws; every failure becomes [Verdict.Failed].
     */
    fun turn(endpoint: String, token: String, turnId: String, wav: ByteArray, budgetMs: Long): Call {
        val t0 = System.nanoTime()
        fun rtt() = (System.nanoTime() - t0) / 1_000_000
        val conn = try {
            URL(endpoint).openConnection() as HttpURLConnection
        } catch (_: IOException) {
            return Call(Verdict.Failed("bad_endpoint"), 0, rtt())
        }
        try {
            conn.requestMethod = "POST"
            conn.connectTimeout = connectTimeoutMs
            conn.readTimeout = budgetMs.coerceIn(1_000, 60_000).toInt()
            conn.doOutput = true
            conn.setFixedLengthStreamingMode(wav.size)
            conn.setRequestProperty("Authorization", "Bearer $token")
            conn.setRequestProperty("Content-Type", "audio/wav")
            conn.setRequestProperty("Accept", "application/json")
            conn.setRequestProperty("X-Weftos-Endpoint", "glasses")
            conn.setRequestProperty("X-Weftos-Turn", turnId)
            conn.outputStream.use { it.write(wav) }
            val status = conn.responseCode
            val stream = if (status in 200..299) conn.inputStream else conn.errorStream
            val body = stream?.use { it.readBytes().toString(Charsets.UTF_8) }.orEmpty()
            return Call(SpeechResponse.parse(status, body), status, rtt())
        } catch (_: SocketTimeoutException) {
            return Call(Verdict.Failed("timeout"), 0, rtt())
        } catch (_: IOException) {
            return Call(Verdict.Failed("network"), 0, rtt())
        } finally {
            conn.disconnect()
        }
    }
}
