package com.weftos.glasses

import java.io.FileOutputStream
import java.io.IOException
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import uniffi.weftos_node_core.LedColor
import uniffi.weftos_node_core.LedPattern

/** One JSON line per command to the BES co-processor (design 3.5, 3.6). Pure rendering. */
object LedCommands {
    fun claim() = "{\"C\":\"android_control_led\",\"V\":1,\"B\":\"1\"}\n"
    fun release() = "{\"C\":\"android_control_led\",\"V\":1,\"B\":\"0\"}\n"
    fun off() = "{\"C\":\"cs_ledoff\",\"V\":1,\"B\":\"\"}\n"
    fun i2sStart() = "{\"C\":\"mh_starti2s\",\"V\":1,\"B\":\"\"}\n"
    fun i2sStop() = "{\"C\":\"mh_stopi2s\",\"V\":1,\"B\":\"\"}\n"

    /** colours: red 0, green 1, blue 2, orange 3, white 4. */
    fun colorCode(c: LedColor): Int = when (c) {
        LedColor.RED -> 0
        LedColor.GREEN -> 1
        LedColor.BLUE -> 2
        LedColor.ORANGE -> 3
        LedColor.WHITE -> 4
    }

    fun set(p: LedPattern, order: LedFieldOrder): String {
        val c = colorCode(p.color)
        val args = when (order) {
            LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS -> "$c,${p.onMs},${p.offMs},${p.cycles},${p.brightness}"
            LedFieldOrder.COLOR_BRIGHTNESS_ON_OFF_CYCLES -> "$c,${p.brightness},${p.onMs},${p.offMs},${p.cycles}"
        }
        return "{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"$args\"}\n"
    }
}

/** Where whole lines go. The real one is the BES serial node; tests inject a fake. */
fun interface LineSink {
    @Throws(IOException::class)
    fun writeLine(line: String)
}

/**
 * Write-only serial sink. asg_client holds the port's reader, so we never read; each line
 * is one open/write/flush/close, the same pattern AudioBridge proved on the device.
 */
class SerialLineSink(private val path: String) : LineSink {
    override fun writeLine(line: String) {
        FileOutputStream(path).use {
            it.write(line.toByteArray(Charsets.UTF_8))
            it.flush()
        }
    }
}

/** Outcome of the latest serial write, for the status board. */
data class SerialResult(val ok: Boolean, val error: String?)

/**
 * Single writer thread for everything that goes to the serial port (LED and I2S lines),
 * so lines are whole and never interleave among our own writers. A failed write is
 * remembered and reported, never thrown at the caller.
 */
class SerialQueue(private val sink: LineSink, private val onResult: (SerialResult) -> Unit = {}) {
    private val exec = Executors.newSingleThreadExecutor { r -> Thread(r, "glasses-serial").apply { isDaemon = true } }

    /** Queue lines in order; returns false if the executor is already shut down. */
    fun submit(vararg lines: String, done: (() -> Unit)? = null): Boolean = try {
        exec.execute {
            for (l in lines) {
                try {
                    sink.writeLine(l)
                    onResult(SerialResult(true, null))
                } catch (e: Exception) { // IOException, SecurityException, ...
                    onResult(SerialResult(false, e.message ?: e.javaClass.simpleName))
                }
            }
            done?.invoke()
        }
        true
    } catch (e: java.util.concurrent.RejectedExecutionException) {
        false
    }

    /** Wait until everything queued so far has been written (or [timeoutMs] passes). */
    fun await(timeoutMs: Long): Boolean {
        val latch = java.util.concurrent.CountDownLatch(1)
        if (!submit(done = { latch.countDown() })) return false
        return latch.await(timeoutMs, TimeUnit.MILLISECONDS)
    }

    fun shutdown() {
        exec.shutdown()
    }
}

/**
 * Drives the RGB LED for indicator states. Claim authority only while a state is shown and
 * release it when idle. A failed write flips [available] to false (status `led:"unavailable"`);
 * the cue tones carry the states meanwhile, and the next state change tries again.
 */
class LedWriter(
    private val serial: SerialQueue,
    @Volatile var order: LedFieldOrder = LedFieldOrder.COLOR_ON_OFF_CYCLES_BRIGHTNESS,
    @Volatile var enabled: Boolean = true,
) {
    private var claimed = false

    @Volatile
    var available: Boolean = true
        private set

    @Volatile
    var lastError: String? = null
        private set

    @Volatile
    var lastLine: String? = null
        private set

    /** Fed by the serial queue's result callback. */
    fun onSerialResult(r: SerialResult) {
        if (r.ok) {
            available = true
            lastError = null
        } else {
            available = false
            lastError = r.error
        }
    }

    @Synchronized
    fun show(p: LedPattern) {
        if (!enabled) return
        val lines = ArrayList<String>(2)
        if (!claimed) {
            lines.add(LedCommands.claim())
            claimed = true
        }
        val set = LedCommands.set(p, order)
        lines.add(set)
        lastLine = set.trim()
        serial.submit(*lines.toTypedArray())
    }

    /** Back to idle: LED off and authority returned to the BES firmware. */
    @Synchronized
    fun release() {
        if (!enabled || !claimed) return
        claimed = false
        serial.submit(LedCommands.off(), LedCommands.release())
    }
}
