package com.weftos.glasses

/** One snapshot of everything the P0 gates need to read off the device. */
data class StatusSnapshot(
    val version: String,
    val uptimeS: Long,
    val link: String, // "down" | "connecting" | "up"
    val parent: String, // host:port or "none"
    val indicator: String,
    val pairing: String,
    /** Six-digit code to speak or show while pairing, else empty. */
    val sas: String = "",
    val micState: String, // "off" | "recording" | "error: ..."
    val micRms: Int,
    val micPeak: Int,
    /** Milliseconds since the last non-zero sample; large means the mic returns silence (G0). */
    val micSilentMs: Long,
    val gateOpen: Boolean,
    val speaking: Boolean,
    val led: String, // "ok" | "unavailable"
    val ledError: String? = null,
    val ledLastLine: String? = null,
    val framesUp: Long,
    val framesDown: Long,
    val reconnects: Long,
    val firstFrameToWriteMs: Long,
    val nowEpochMs: Long,
) {
    fun toJson(): String {
        val sb = StringBuilder("{")
        var first = true
        fun kv(k: String, v: String) {
            if (!first) sb.append(',')
            first = false
            sb.append('"').append(k).append("\":").append(v)
        }
        fun s(v: String?) = if (v == null) "null" else "\"" + esc(v) + "\""
        kv("ver", s(version))
        kv("uptime_s", uptimeS.toString())
        kv("link", s(link))
        kv("parent", s(parent))
        kv("indicator", s(indicator))
        kv("pairing", s(pairing))
        kv("sas", s(sas))
        kv("mic", s(micState))
        kv("mic_rms", micRms.toString())
        kv("mic_peak", micPeak.toString())
        kv("mic_silent_ms", micSilentMs.toString())
        kv("gate_open", gateOpen.toString())
        kv("speaking", speaking.toString())
        kv("led", s(led))
        kv("led_error", s(ledError))
        kv("led_last_line", s(ledLastLine))
        kv("frames_up", framesUp.toString())
        kv("frames_down", framesDown.toString())
        kv("reconnects", reconnects.toString())
        kv("first_frame_to_write_ms", firstFrameToWriteMs.toString())
        kv("ts_ms", nowEpochMs.toString())
        return sb.append('}').toString()
    }

    /** The one-line form for logcat. */
    fun logLine(): String =
        "link=$link ind=$indicator mic=$micState rms=$micRms silent_ms=$micSilentMs gate=${if (gateOpen) "open" else "closed"} " +
            "spk=$speaking led=$led up=$framesUp down=$framesDown pair=$pairing"

    companion object {
        fun esc(v: String): String {
            val sb = StringBuilder()
            for (c in v) when {
                c == '"' -> sb.append("\\\"")
                c == '\\' -> sb.append("\\\\")
                c == '\n' -> sb.append("\\n")
                c == '\r' -> sb.append("\\r")
                c == '\t' -> sb.append("\\t")
                c.code < 0x20 -> sb.append(String.format("\\u%04x", c.code))
                else -> sb.append(c)
            }
            return sb.toString()
        }
    }
}
