package com.weftos.glasses

import android.content.Context
import android.os.PowerManager
import android.os.SystemClock
import android.util.Log
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit
import uniffi.weftos_node_core.CaptureMode
import uniffi.weftos_node_core.Codec
import uniffi.weftos_node_core.GlassesCore
import uniffi.weftos_node_core.GlassesPairingState
import uniffi.weftos_node_core.GlassesStatus
import uniffi.weftos_node_core.Handled
import uniffi.weftos_node_core.IndicatorAction
import uniffi.weftos_node_core.IndicatorInput
import uniffi.weftos_node_core.IndicatorState
import uniffi.weftos_node_core.LedColor
import uniffi.weftos_node_core.LedPattern
import uniffi.weftos_node_core.ShellCommand

/**
 * The whole glasses agent: mic -> gate -> Rust core -> TCP -> phone, and phone -> core ->
 * speaker / LED / cues. Protocol decisions (framing, command routing, the indicator state
 * machine) live in `weftos-node-core`; this class only performs what the core returns.
 */
class Agent(private val ctx: Context, config: GlassesConfig) {
    @Volatile var config: GlassesConfig = config
        private set

    private val startedAt = SystemClock.elapsedRealtime()
    private val env = Environment(ctx)
    private val lock = Any()
    private var core: GlassesCore? = null

    @Volatile private var coreError: String? = null
    private val sched = Executors.newSingleThreadScheduledExecutor { r -> Thread(r, "glasses-sched").apply { isDaemon = true } }
    private var tickFuture: ScheduledFuture<*>? = null
    private var thinkFuture: ScheduledFuture<*>? = null
    private var statusFuture: ScheduledFuture<*>? = null
    private var wakeLock: PowerManager.WakeLock? = null

    private lateinit var led: LedWriter
    private val serial = SerialQueue(SerialLineSink(config.serialPath)) { r -> if (::led.isInitialized) led.onSerialResult(r) }
    private val cues = HashMap<CueAsset, ByteArray>()
    private val gate = EnergyGate(config.gate)

    private val playback = Playback(
        serial, config.audioUsage, config.i2sControl,
        onSpeaking = { on -> if (!on) speakEndedAt = SystemClock.elapsedRealtime() },
        onSpeechDrained = { utt, seq -> sendFrame { it.playbackDoneFrame(utt.toUInt(), seq.toUInt()) } },
    )
    private val mic = MicCapture(onFrame = { f, ts -> onMicFrame(f, ts) }, onState = { micState = it })
    private val link = ParentLink(
        hostOf = { this.config.parentHost.ifEmpty { env.gateway() } },
        portOf = { this.config.parentPort },
        onUp = { onLinkUp() },
        onDown = { onLinkDown() },
        onBytes = { b -> onLinkBytes(b) },
    )

    @Volatile private var micState = "off"
    @Volatile private var speakEndedAt = 0L
    @Volatile private var framesUp = 0L
    @Volatile private var framesDown = 0L
    private var utt = 0
    private var seq = 0

    private fun now(): ULong = SystemClock.elapsedRealtime().toULong()

    fun start() {
        led = LedWriter(serial, config.ledOrder, config.ledEnabled)
        loadCues()
        applyVolumes()
        try {
            core = GlassesCore(File(ctx.filesDir, "weft-glasses").also { it.mkdirs() }.absolutePath)
        } catch (t: Throwable) { // UnsatisfiedLinkError, or a core error
            coreError = t.message ?: t.javaClass.simpleName
            Log.e(TAG, "native core unavailable: $coreError", t)
        }
        if (config.wakeLock) {
            val pm = ctx.getSystemService(PowerManager::class.java)
            wakeLock = pm?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "weftos:glasses")?.apply {
                setReferenceCounted(false)
                acquire()
            }
        }
        mic.enabled = config.captureOnStart
        playback.start()
        mic.start()
        if (core != null) link.start()
        statusFuture = sched.scheduleWithFixedDelay({ publishStatus() }, 1, STATUS_PERIOD_S, TimeUnit.SECONDS)
        Log.i(TAG, "agent started core=${core != null} node=${core?.nodeId()}")
    }

    fun stop() {
        statusFuture?.cancel(false)
        tickFuture?.cancel(false)
        thinkFuture?.cancel(false)
        link.stop()
        mic.stop()
        playback.stop()
        led.release()
        serial.await(500)
        serial.shutdown()
        sched.shutdownNow()
        try { wakeLock?.release() } catch (_: Exception) { }
        core?.close()
        core = null
        Log.i(TAG, "agent stopped")
    }

    private fun loadCues() {
        for (c in CueAsset.values()) {
            try {
                val wav = Wav.parse(ctx.assets.open(c.file).use { it.readBytes() })
                if (wav.sampleRate == AudioFormatP0.SAMPLE_RATE) cues[c] = wav.data
                else Log.w(TAG, "cue ${c.file} is ${wav.sampleRate} Hz, expected ${AudioFormatP0.SAMPLE_RATE}")
            } catch (e: Exception) {
                Log.w(TAG, "cue ${c.file} not loaded: ${e.message}")
            }
        }
    }

    private fun applyVolumes() {
        playback.voiceVolume = config.voiceVolume
        playback.cueVolume = config.cueVolume
    }

    /** Live-apply what can change without a restart; the caller persists the config. */
    fun updateConfig(c: GlassesConfig) {
        config = c
        applyVolumes()
        led.order = c.ledOrder
        led.enabled = c.ledEnabled
    }

    // ---- indicator and core plumbing -------------------------------------------------

    fun indicate(input: IndicatorInput) {
        val c = core ?: return
        synchronized(lock) {
            try {
                apply(c.feed(now(), input))
            } catch (e: Exception) {
                Log.w(TAG, "indicator feed failed: ${e.message}")
            }
            rescheduleTick(c)
        }
    }

    private fun onLinkBytes(bytes: ByteArray) {
        val c = core ?: return
        synchronized(lock) {
            apply(c.onBytes(bytes, now())) // a core error closes the socket via ParentLink
            rescheduleTick(c)
        }
    }

    private fun rescheduleTick(c: GlassesCore) {
        tickFuture?.cancel(false)
        val d = c.nextDeadline() ?: return
        val delay = (d.toLong() - SystemClock.elapsedRealtime()).coerceAtLeast(0)
        try {
            tickFuture = sched.schedule({ indicate(IndicatorInput.Tick) }, delay, TimeUnit.MILLISECONDS)
        } catch (_: java.util.concurrent.RejectedExecutionException) { }
    }

    private fun apply(h: Handled) {
        for (f in h.send) link.send(f)
        for (a in h.actions) doAction(a)
        for (c in h.commands) doCommand(c)
        for (fr in h.audio) playFrame(fr)
    }

    private fun doAction(a: IndicatorAction) {
        when (a) {
            is IndicatorAction.PlayCue -> playCue(CueMap.forIndicator(a.cue))
            is IndicatorAction.StartThinkingLoop -> startThinkingLoop(a.periodMs.toLong())
            is IndicatorAction.StopThinkingLoop -> stopThinkingLoop()
            is IndicatorAction.SetLed -> led.show(a.pattern)
            is IndicatorAction.ReleaseLed -> led.release()
            is IndicatorAction.Report -> Unit // already sent as a frame by the core
        }
    }

    private fun startThinkingLoop(periodMs: Long) {
        stopThinkingLoop()
        try {
            thinkFuture = sched.scheduleAtFixedRate({ playCue(CueAsset.THINKING_TICK) }, 700, periodMs, TimeUnit.MILLISECONDS)
        } catch (_: java.util.concurrent.RejectedExecutionException) { }
    }

    private fun stopThinkingLoop() {
        thinkFuture?.cancel(false)
        thinkFuture = null
    }

    fun playCue(a: CueAsset) {
        if (!config.cuesEnabled) return
        val pcm = cues[a] ?: return
        playback.enqueueCue(pcm)
    }

    private fun doCommand(c: ShellCommand) {
        when (c) {
            is ShellCommand.CaptureStart -> {
                mic.enabled = true
                val forced = c.mode != CaptureMode.VAD
                gate.force(forced)
                if (c.mode == CaptureMode.PTT) indicate(IndicatorInput.PttStart)
                if (forced && c.maxS > 0u) {
                    sched.schedule({ doCommand(ShellCommand.CaptureStop) }, c.maxS.toLong(), TimeUnit.SECONDS)
                }
            }
            is ShellCommand.CaptureStop -> {
                gate.force(false)
                if (gate.isOpen) {
                    gate.reset()
                    indicate(IndicatorInput.GateClose)
                }
            }
            is ShellCommand.PlayBegin -> Unit
            is ShellCommand.PlayFlush -> playback.flush(c.uttId.toInt())
            is ShellCommand.VolumeSet -> playback.voiceVolume = c.level.coerceIn(0f, 1f)
            is ShellCommand.CueVolumeSet -> playback.cueVolume = c.level.coerceIn(0f, 1f)
            is ShellCommand.Cue -> playCue(CueMap.forKind(c.kind))
            is ShellCommand.LedSet -> led.show(c.pattern)
            is ShellCommand.Probe -> Log.i(TAG, "probe ${c.kind}: ${snapshot().logLine()}")
        }
    }

    private fun playFrame(f: uniffi.weftos_node_core.AudioFrame) {
        framesDown++
        if (f.codec == Codec.PCM16) {
            playback.enqueueSpeech(f.utt.toInt(), f.seq.toInt(), f.payload)
        } else if (framesDown % 50L == 1L) {
            Log.w(TAG, "downlink codec ${f.codec} is not played in P0 (PCM16 only)")
        }
    }

    // ---- link and mic callbacks --------------------------------------------------------

    private fun onLinkUp() {
        val c = core ?: return
        synchronized(lock) {
            c.resetStream()
            link.send(c.linkUpFrame())
            val paired = c.pairingState() is GlassesPairingState.Paired
            if (!paired && config.sendPairHello) link.send(c.pairHello())
        }
        publishStatus()
    }

    private fun onLinkDown() {
        gate.reset()
        indicate(IndicatorInput.LinkLost)
        publishStatus()
    }

    private fun uplinkAllowed(c: GlassesCore): Boolean =
        config.allowUnpaired || c.pairingState() is GlassesPairingState.Paired

    private fun onMicFrame(frame: ByteArray, tsMs: Long) {
        val c = core ?: return
        val cfg = config
        val halfDuplexBlock = cfg.halfDuplex &&
            (playback.speaking || SystemClock.elapsedRealtime() - speakEndedAt < HALF_DUPLEX_TAIL_MS)
        if (!link.isUp || !uplinkAllowed(c) || halfDuplexBlock) {
            if (gate.isOpen) {
                gate.reset()
                indicate(IndicatorInput.GateClose)
            }
            return
        }
        val r = gate.process(frame)
        if (r.opened) {
            utt++
            seq = 0
            indicate(IndicatorInput.GateOpen)
            sendFrame { it.micGateFrame(true, r.rms.toUInt(), 0f) }
        }
        for (f in r.send) {
            sendFrame { it.audioUpPcmFrame(utt.toUInt(), seq.toUInt(), (seq * AudioFormatP0.FRAME_MS).toUInt(), f) }
            seq++
            framesUp++
        }
        if (r.closed) {
            indicate(IndicatorInput.GateClose)
            sendFrame { it.micGateFrame(false, r.rms.toUInt(), 0f) }
        }
    }

    private inline fun sendFrame(build: (GlassesCore) -> ByteArray) {
        val c = core ?: return
        try {
            link.send(build(c))
        } catch (e: Exception) {
            Log.w(TAG, "frame build failed: ${e.message}")
        }
    }

    // ---- status ------------------------------------------------------------------------

    fun snapshot(): StatusSnapshot {
        val c = core
        val pairing = when (val p = c?.pairingState()) {
            null -> "no-core"
            is GlassesPairingState.Idle -> "idle"
            is GlassesPairingState.AwaitingParent -> "awaiting-parent"
            is GlassesPairingState.AwaitingCert -> "awaiting-confirm"
            is GlassesPairingState.Paired -> "paired"
            is GlassesPairingState.Refused -> "refused: ${p.reason}"
        }
        val sas = (c?.pairingState() as? GlassesPairingState.AwaitingCert)?.sas ?: ""
        return StatusSnapshot(
            version = BuildConfigLite.VERSION,
            uptimeS = (SystemClock.elapsedRealtime() - startedAt) / 1000,
            link = link.state,
            parent = link.peer,
            indicator = c?.indicatorState()?.name?.lowercase() ?: "no-core",
            pairing = pairing,
            sas = sas,
            micState = if (coreError != null) "core unavailable: $coreError" else micState,
            micRms = mic.level,
            micPeak = mic.peak,
            micSilentMs = mic.silentMs,
            gateOpen = gate.isOpen,
            speaking = playback.speaking,
            led = if (led.available) "ok" else "unavailable",
            ledError = led.lastError,
            ledLastLine = led.lastLine,
            framesUp = framesUp,
            framesDown = framesDown,
            reconnects = link.reconnects,
            firstFrameToWriteMs = playback.lastFirstFrameToWriteMs,
            nowEpochMs = System.currentTimeMillis(),
        )
    }

    /** Log line plus `status.json` (pullable over adb) plus a status frame to the parent. */
    fun publishStatus() {
        val s = snapshot()
        Log.i(TAG, s.logLine())
        try {
            val dir = ctx.getExternalFilesDir(null) ?: ctx.filesDir
            val tmp = File(dir, ".status.json.tmp")
            tmp.writeText(s.toJson())
            tmp.renameTo(File(dir, "status.json"))
        } catch (e: Exception) {
            Log.w(TAG, "status.json: ${e.message}")
        }
        if (link.isUp) {
            val b = env.battery()
            val w = env.wifi()
            sendFrame {
                it.statusFrame(
                    GlassesStatus(
                        battery = b.percent.toUByte(),
                        charging = b.charging,
                        wifiRssi = w.rssi.toShort(),
                        wifiRateMbps = w.rateMbps.coerceIn(0, 65535).toUShort(),
                        micOpen = gate.isOpen,
                        micLevel = mic.level.coerceIn(0, 65535).toUShort(),
                        speaking = playback.speaking,
                        ledAvailable = led.available,
                        ver = BuildConfigLite.VERSION,
                    ),
                )
            }
        }
    }

    // ---- shell-driven checks (no phone needed) -------------------------------------------

    /** Walk the indicator states locally so cues and LED can be checked by eye and ear. */
    fun demo() {
        sched.execute {
            indicate(IndicatorInput.GateOpen)
            Thread.sleep(1500)
            indicate(IndicatorInput.GateClose) // captured, then thinking after 400 ms
            Thread.sleep(3000)
            indicate(IndicatorInput.Set(IndicatorState.DONE_OK, 9001uL, 1uL))
            Thread.sleep(2500)
            indicate(IndicatorInput.GateOpen)
            Thread.sleep(1000)
            indicate(IndicatorInput.GateClose)
            Thread.sleep(2500)
            indicate(IndicatorInput.Set(IndicatorState.DONE_ERROR, 9002uL, 1uL))
        }
    }

    fun ledTest(p: LedPattern) {
        led.show(p)
    }

    fun ledOff() {
        led.release()
    }

    fun colorOf(code: Int): LedColor = LedColor.values().getOrElse(code) { LedColor.WHITE }

    companion object {
        const val TAG = "WeftGlasses"
        private const val STATUS_PERIOD_S = 5L

        /** Mic stays gated shut this long after the speaker stops (room tail, no AEC yet). */
        private const val HALF_DUPLEX_TAIL_MS = 300L
    }
}

/** Version string without BuildConfig (Gradle's generation is off for this module). */
object BuildConfigLite {
    const val VERSION = "0.1.0-p0"
}
