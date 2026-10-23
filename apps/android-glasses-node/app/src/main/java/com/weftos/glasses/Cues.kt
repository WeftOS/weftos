package com.weftos.glasses

import uniffi.weftos_node_core.CueKind
import uniffi.weftos_node_core.Cue as IndicatorCue

/** The bundled cue assets (`assets/cues/<name>.wav`, rendered by `scripts/gen_cues.py`). */
enum class CueAsset(val file: String) {
    LISTENING_START("cues/listening_start.wav"),
    CAPTURED("cues/captured.wav"),
    THINKING_TICK("cues/thinking_tick.wav"),
    DONE_OK("cues/done_ok.wav"),
    DONE_ERROR("cues/done_error.wav");

    companion object {
        fun byName(n: String): CueAsset? = when (n.trim().lowercase()) {
            "listening", "listening_start", "heard", "ack" -> LISTENING_START
            "captured" -> CAPTURED
            "thinking", "thinking_tick", "working" -> THINKING_TICK
            "done", "done_ok", "ok" -> DONE_OK
            "error", "done_error" -> DONE_ERROR
            else -> null
        }
    }
}

/** Which asset each indicator cue / protocol cue kind plays. */
object CueMap {
    fun forIndicator(c: IndicatorCue): CueAsset = when (c) {
        IndicatorCue.LISTENING_START -> CueAsset.LISTENING_START
        IndicatorCue.CAPTURED -> CueAsset.CAPTURED
        IndicatorCue.DONE_OK -> CueAsset.DONE_OK
        IndicatorCue.DONE_ERROR -> CueAsset.DONE_ERROR
    }

    /** The `cue{kind}` command vocabulary of design 3.6. */
    fun forKind(k: CueKind): CueAsset = when (k) {
        CueKind.HEARD -> CueAsset.LISTENING_START
        CueKind.WORKING -> CueAsset.THINKING_TICK
        CueKind.DONE -> CueAsset.DONE_OK
        CueKind.ERROR -> CueAsset.DONE_ERROR
    }
}

/** Minimal RIFF/WAVE reader for the bundled cues: returns PCM16 mono payload bytes. */
object Wav {
    class Pcm16(val sampleRate: Int, val data: ByteArray)

    fun parse(b: ByteArray): Pcm16 {
        require(b.size >= 44 && String(b, 0, 4, Charsets.US_ASCII) == "RIFF" && String(b, 8, 4, Charsets.US_ASCII) == "WAVE") {
            "not a RIFF/WAVE file"
        }
        var pos = 12
        var rate = 0
        var bits = 0
        var channels = 0
        while (pos + 8 <= b.size) {
            val id = String(b, pos, 4, Charsets.US_ASCII)
            val len = le32(b, pos + 4)
            val body = pos + 8
            if (id == "fmt ") {
                require(le16(b, body) == 1) { "not PCM" }
                channels = le16(b, body + 2)
                rate = le32(b, body + 4)
                bits = le16(b, body + 14)
            } else if (id == "data") {
                require(bits == 16 && channels == 1) { "cue must be mono PCM16" }
                val end = minOf(b.size, body + len)
                return Pcm16(rate, b.copyOfRange(body, end))
            }
            pos = body + len + (len and 1)
        }
        throw IllegalArgumentException("no data chunk")
    }

    private fun le16(b: ByteArray, o: Int) = (b[o].toInt() and 0xff) or ((b[o + 1].toInt() and 0xff) shl 8)
    private fun le32(b: ByteArray, o: Int) = le16(b, o) or (le16(b, o + 2) shl 16)
}
