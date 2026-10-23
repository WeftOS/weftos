package com.weftos.glasses

data class GateConfig(
    /** Absolute RMS (PCM16 units) needed to open. */
    val openRms: Int = 400,
    /** Absolute RMS below which the gate may close (after the hangover). */
    val closeRms: Int = 250,
    /** The gate also needs this multiple of the tracked noise floor to open. */
    val openRatio: Float = 3f,
    val closeRatio: Float = 2f,
    /** Consecutive loud frames needed to open (kills clicks). */
    val openFrames: Int = 2,
    val hangoverMs: Int = 800,
    /** Audio kept from before the gate opened, so word starts are never clipped. */
    val prerollMs: Int = 400,
    /** Hard cap on one open stretch (cost and bandwidth limit, design 4.2). */
    val maxOpenMs: Int = 30_000,
)

/** What one frame did to the gate. [send] is what goes upstream, oldest first. */
data class GateResult(
    val rms: Int,
    val opened: Boolean,
    val closed: Boolean,
    val send: List<ByteArray>,
    val open: Boolean,
)

/**
 * Energy gate with a calibrated noise floor and a pre-roll ring buffer (design 4.2).
 * Pure: no clocks, no Android. One call per 20 ms frame; time comes from the frame count.
 */
class EnergyGate(private val cfg: GateConfig = GateConfig(), private val frameMs: Int = AudioFormatP0.FRAME_MS) {
    private val prerollFrames = (cfg.prerollMs / frameMs).coerceAtLeast(0)
    private val hangoverFrames = (cfg.hangoverMs / frameMs).coerceAtLeast(0)
    private val maxOpenFrames = (cfg.maxOpenMs / frameMs).coerceAtLeast(1)
    private val ring = ArrayDeque<ByteArray>()
    private var loudRun = 0
    private var quietRun = 0
    private var openFramesCount = 0
    private var open = false
    private var forced = false

    /** Tracked ambient level while closed; -1 until the first sub-threshold frame calibrates it. */
    var noiseFloor: Float = -1f
        private set

    val isOpen: Boolean get() = open

    /** Push-to-talk / continuous: hold the gate open regardless of level. */
    fun force(on: Boolean) {
        forced = on
    }

    fun reset() {
        ring.clear()
        loudRun = 0
        quietRun = 0
        openFramesCount = 0
        open = false
    }

    private fun openThreshold(): Float = maxOf(cfg.openRms.toFloat(), noiseFloor.coerceAtLeast(0f) * cfg.openRatio)
    private fun closeThreshold(): Float = maxOf(cfg.closeRms.toFloat(), noiseFloor.coerceAtLeast(0f) * cfg.closeRatio)

    fun process(frame: ByteArray): GateResult {
        val rms = Pcm.rms(frame)
        var opened = false
        var closed = false
        val send = ArrayList<ByteArray>()

        if (!open) {
            // Calibrate on quiet audio only, slowly, so speech (even speech that is already
            // under way when the agent starts) never raises the floor.
            if (rms < openThreshold()) {
                noiseFloor = if (noiseFloor < 0f) rms.toFloat() else noiseFloor + (rms - noiseFloor) * 0.02f
            }

            loudRun = if (forced || rms >= openThreshold()) loudRun + 1 else 0
            if (loudRun >= (if (forced) 1 else cfg.openFrames)) {
                open = true
                opened = true
                openFramesCount = 0
                quietRun = 0
                send.addAll(ring)
                ring.clear()
                send.add(frame)
                openFramesCount = send.size
            } else {
                ring.addLast(frame)
                while (ring.size > prerollFrames) ring.removeFirst()
            }
        } else {
            send.add(frame)
            openFramesCount++
            quietRun = if (!forced && rms < closeThreshold()) quietRun + 1 else 0
            if (quietRun > hangoverFrames || openFramesCount >= maxOpenFrames) {
                open = false
                closed = true
                loudRun = 0
                quietRun = 0
            }
        }
        return GateResult(rms, opened, closed, send, open)
    }
}
