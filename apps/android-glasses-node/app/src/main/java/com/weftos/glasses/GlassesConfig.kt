package com.weftos.glasses

/** Order of the `cs_ledon` argument string; the glasses' own notes disagree (design 3.6). */
enum class LedFieldOrder {
    /** `color,on_ms,off_ms,cycles,brightness` (UART protocol doc, primary). */
    COLOR_ON_OFF_CYCLES_BRIGHTNESS,

    /** `color,brightness,on_ms,off_ms,cycles` (testing notes). */
    COLOR_BRIGHTNESS_ON_OFF_CYCLES;

    companion object {
        fun parse(s: String?): LedFieldOrder = when (s?.trim()?.lowercase()) {
            "b", "brightness_first", "color_brightness_on_off_cycles" -> COLOR_BRIGHTNESS_ON_OFF_CYCLES
            else -> COLOR_ON_OFF_CYCLES_BRIGHTNESS
        }
    }
}

/**
 * Everything tunable, as strings so it can arrive as intent extras
 * (`--es parent_host ...`) and persist in SharedPreferences. No addresses are baked in:
 * with no `parent_host` the agent looks for the Wi-Fi default gateway, which on the
 * phone's hotspot is the phone.
 */
data class GlassesConfig(
    val parentHost: String = "",
    val parentPort: Int = DEFAULT_PORT,
    /** Start capturing as soon as the service starts (G0 soak needs no phone command). */
    val captureOnStart: Boolean = true,
    /** P0 dev mode: stream audio before pairing completes (no Noise channel yet). */
    val allowUnpaired: Boolean = true,
    val sendPairHello: Boolean = true,
    val gate: GateConfig = GateConfig(),
    /** Ignore the mic gate while the speaker plays a reply (no AEC until G3). */
    val halfDuplex: Boolean = true,
    val voiceVolume: Float = 1.0f,
    /** Relative to voice volume (design 3.7: default 0.4 of it). */
    val cueVolume: Float = 0.4f,
    val cuesEnabled: Boolean = true,
    val ledEnabled: Boolean = true,
    val ledOrder: LedFieldOrder = LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS,
    val serialPath: String = "/dev/ttyS1",
    /** `notification` (what AudioBridge proved on this device), `media` or `assistant`. */
    val audioUsage: String = "notification",
    /** Send the BES `mh_starti2s`/`mh_stopi2s` lines around playback (as AudioBridge does). */
    val i2sControl: Boolean = true,
    /** Hold a partial wake lock so capture survives display-off idle (soak, gate A2). */
    val wakeLock: Boolean = true,
) {
    companion object {
        const val DEFAULT_PORT = 7440

        fun fromMap(m: Map<String, String>, base: GlassesConfig = GlassesConfig()): GlassesConfig {
            fun str(k: String, d: String) = m[k]?.trim()?.takeIf { it.isNotEmpty() } ?: d
            fun bool(k: String, d: Boolean) = when (m[k]?.trim()?.lowercase()) {
                "1", "true", "yes", "on" -> true
                "0", "false", "no", "off" -> false
                else -> d
            }
            fun int(k: String, d: Int) = m[k]?.trim()?.toIntOrNull() ?: d
            fun flt(k: String, d: Float) = (m[k]?.trim()?.toFloatOrNull() ?: d).coerceIn(0f, 1f)
            val g = base.gate
            return GlassesConfig(
                parentHost = if (m.containsKey("parent_host")) m["parent_host"]!!.trim() else base.parentHost,
                parentPort = int("parent_port", base.parentPort).takeIf { it in 1..65535 } ?: base.parentPort,
                captureOnStart = bool("capture_on_start", base.captureOnStart),
                allowUnpaired = bool("allow_unpaired", base.allowUnpaired),
                sendPairHello = bool("send_pair_hello", base.sendPairHello),
                gate = GateConfig(
                    openRms = int("gate_open_rms", g.openRms),
                    closeRms = int("gate_close_rms", g.closeRms),
                    openRatio = flt10("gate_open_ratio", m, g.openRatio),
                    closeRatio = flt10("gate_close_ratio", m, g.closeRatio),
                    openFrames = int("gate_open_frames", g.openFrames).coerceAtLeast(1),
                    hangoverMs = int("gate_hangover_ms", g.hangoverMs).coerceAtLeast(0),
                    prerollMs = int("gate_preroll_ms", g.prerollMs).coerceAtLeast(0),
                    maxOpenMs = int("gate_max_open_ms", g.maxOpenMs).coerceAtLeast(1000),
                ),
                halfDuplex = bool("half_duplex", base.halfDuplex),
                voiceVolume = flt("voice_volume", base.voiceVolume),
                cueVolume = flt("cue_volume", base.cueVolume),
                cuesEnabled = bool("cues", base.cuesEnabled),
                ledEnabled = bool("led", base.ledEnabled),
                ledOrder = if (m.containsKey("led_order")) LedFieldOrder.parse(m["led_order"]) else base.ledOrder,
                serialPath = str("serial_path", base.serialPath),
                audioUsage = str("audio_usage", base.audioUsage).lowercase(),
                i2sControl = bool("i2s_control", base.i2sControl),
                wakeLock = bool("wake_lock", base.wakeLock),
            )
        }

        // Ratios may exceed 1, so they are not clamped to 0..1 like volumes.
        private fun flt10(k: String, m: Map<String, String>, d: Float) =
            (m[k]?.trim()?.toFloatOrNull() ?: d).coerceIn(1f, 20f)
    }

    /** Serialise for SharedPreferences (only non-secret tunables exist here). */
    fun toMap(): Map<String, String> = buildMap {
        put("parent_host", parentHost)
        put("parent_port", parentPort.toString())
        put("capture_on_start", captureOnStart.toString())
        put("allow_unpaired", allowUnpaired.toString())
        put("send_pair_hello", sendPairHello.toString())
        put("gate_open_rms", gate.openRms.toString())
        put("gate_close_rms", gate.closeRms.toString())
        put("gate_open_ratio", gate.openRatio.toString())
        put("gate_close_ratio", gate.closeRatio.toString())
        put("gate_open_frames", gate.openFrames.toString())
        put("gate_hangover_ms", gate.hangoverMs.toString())
        put("gate_preroll_ms", gate.prerollMs.toString())
        put("gate_max_open_ms", gate.maxOpenMs.toString())
        put("half_duplex", halfDuplex.toString())
        put("voice_volume", voiceVolume.toString())
        put("cue_volume", cueVolume.toString())
        put("cues", cuesEnabled.toString())
        put("led", ledEnabled.toString())
        put("led_order", if (ledOrder == LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS) "a" else "b")
        put("serial_path", serialPath)
        put("audio_usage", audioUsage)
        put("i2s_control", i2sControl.toString())
        put("wake_lock", wakeLock.toString())
    }
}
