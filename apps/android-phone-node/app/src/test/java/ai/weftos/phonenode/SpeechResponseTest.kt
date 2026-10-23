package ai.weftos.phonenode

import java.util.Base64
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SpeechResponseTest {
    private val samples = ByteArray(6_400) { (it % 97).toByte() }
    private val b64 = Base64.getEncoder().encodeToString(samples)

    private fun ok(audio: String = """{"format":"pcm_s16le","rate":16000,"channels":1,"b64":"$b64"}""") =
        """{"status":"ok","turn_id":"t1","transcript":"hello","reply_text":"Hi there","spoken_text":"Hi there.",
           "audio":$audio,"timings_ms":{"stt":310,"llm":950,"tts":420}}"""

    @Test
    fun okCarriesTextAudioAndTimings() {
        val v = SpeechResponse.parse(200, ok()) as Verdict.Reply
        assertEquals("hello", v.transcript)
        assertEquals("Hi there", v.replyText)
        assertEquals("Hi there.", v.spokenText)
        assertArrayEquals(samples, v.pcm)
        assertEquals(mapOf("stt" to 310L, "llm" to 950L, "tts" to 420L), v.timingsMs)
    }

    @Test
    fun okWithoutAudioIsAnEmptyReply() {
        val v = SpeechResponse.parse(200, """{"status":"ok","transcript":"x","reply_text":"y"}""") as Verdict.Reply
        assertEquals(0, v.pcm.size)
    }

    @Test
    fun noSpeechIsItsOwnOutcome() {
        val v = SpeechResponse.parse(200, """{"status":"no_speech","turn_id":"t","transcript":""}""")
        assertTrue(v is Verdict.NoSpeech)
    }

    @Test
    fun errorBodyKeepsItsCode() {
        val v = SpeechResponse.parse(200, """{"status":"error","code":"budget","message":"daily cap reached"}""")
        assertEquals(Verdict.Failed("budget", "daily cap reached"), v)
        val w = SpeechResponse.parse(502, """{"status":"error","code":"stt_failed","message":"upstream"}""")
        assertEquals("stt_failed", (w as Verdict.Failed).code)
    }

    @Test
    fun errorWithoutCodeFallsBackToTheHttpStatus() {
        val v = SpeechResponse.parse(429, """{"status":"error","message":"slow down"}""")
        assertEquals("rate_limited", (v as Verdict.Failed).code)
    }

    @Test
    fun nonJsonBodiesMapByHttpStatus() {
        fun code(status: Int) = (SpeechResponse.parse(status, "<html>nope</html>") as Verdict.Failed).code
        assertEquals("auth", code(401))
        assertEquals("auth", code(403))
        assertEquals("budget", code(402))
        assertEquals("too_large", code(413))
        assertEquals("rate_limited", code(429))
        assertEquals("server", code(500))
        assertEquals("server", code(503))
        assertEquals("http_404", code(404))
        assertEquals("bad_response", code(200))
    }

    @Test
    fun okStatusInsideAnHttpErrorIsNotTrusted() {
        val v = SpeechResponse.parse(500, ok())
        assertEquals("server", (v as Verdict.Failed).code)
    }

    @Test
    fun wrongAudioFormatIsRefusedNotPlayed() {
        val bad = """{"format":"pcm_s16le","rate":24000,"channels":1,"b64":"$b64"}"""
        assertEquals("bad_audio_format", (SpeechResponse.parse(200, ok(bad)) as Verdict.Failed).code)
        val stereo = """{"format":"pcm_s16le","rate":16000,"channels":2,"b64":"$b64"}"""
        assertEquals("bad_audio_format", (SpeechResponse.parse(200, ok(stereo)) as Verdict.Failed).code)
        val mp3 = """{"format":"mp3","rate":16000,"channels":1,"b64":"$b64"}"""
        assertEquals("bad_audio_format", (SpeechResponse.parse(200, ok(mp3)) as Verdict.Failed).code)
    }

    @Test
    fun brokenBase64IsAFailureNotACrash() {
        val bad = """{"format":"pcm_s16le","rate":16000,"channels":1,"b64":"!!not base64!!"}"""
        assertEquals("bad_audio", (SpeechResponse.parse(200, ok(bad)) as Verdict.Failed).code)
    }

    @Test
    fun unknownStatusIsABadResponse() {
        assertEquals("bad_response", (SpeechResponse.parse(200, """{"status":"maybe"}""") as Verdict.Failed).code)
        assertEquals("bad_response", (SpeechResponse.parse(200, "{}") as Verdict.Failed).code)
    }
}
