package com.weftos.glasses

import android.util.Log
import java.io.IOException
import java.net.InetSocketAddress
import java.net.Socket
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * TCP client to the phone (the parent node). Reconnects with a short capped backoff so the
 * link re-forms within seconds of the hotspot returning (gate A4). Frames are already
 * length-prefixed by the Rust core; this class only moves bytes.
 *
 * Discovery is a stub: a configured host wins, otherwise [gatewayLookup] supplies the
 * Wi-Fi default gateway (the phone, when the glasses join the phone's hotspot).
 */
class ParentLink(
    private val hostOf: () -> String?,
    private val portOf: () -> Int,
    private val onUp: () -> Unit,
    private val onDown: () -> Unit,
    private val onBytes: (ByteArray) -> Unit,
) {
    @Volatile var state = "down"
        private set
    @Volatile var peer = "none"
        private set
    @Volatile var reconnects = 0L
        private set

    private val out = LinkedBlockingQueue<ByteArray>(MAX_QUEUE)
    @Volatile private var running = false
    @Volatile private var sock: Socket? = null
    private var thread: Thread? = null

    val isUp: Boolean get() = state == "up"

    fun start() {
        if (thread != null) return
        running = true
        thread = Thread({ loop() }, "glasses-link").also {
            it.isDaemon = true
            it.start()
        }
    }

    fun stop() {
        running = false
        try { sock?.close() } catch (_: IOException) { }
        thread?.interrupt()
        thread?.join(1000)
        thread = null
    }

    /** Queue a frame; false when the link is down or the queue is full (frame dropped). */
    fun send(frame: ByteArray): Boolean {
        if (!isUp) return false
        if (out.offer(frame)) return true
        // Realtime audio: late is worse than lost. Drop the oldest and keep the newest.
        out.poll()
        return out.offer(frame)
    }

    private fun loop() {
        var backoff = 500L
        while (running) {
            val host = hostOf()
            if (host.isNullOrEmpty()) {
                peer = "none"
                state = "down"
                sleep(2000)
                continue
            }
            val port = portOf()
            peer = "$host:$port"
            state = "connecting"
            val s = Socket()
            try {
                s.connect(InetSocketAddress(host, port), CONNECT_TIMEOUT_MS)
                s.tcpNoDelay = true
                sock = s
                out.clear()
                state = "up"
                backoff = 500L
                onUp()
                serve(s)
            } catch (e: Exception) {
                if (running) Log.d(TAG, "link: ${e.message}")
            } finally {
                val wasUp = state == "up"
                state = "down"
                try { s.close() } catch (_: IOException) { }
                sock = null
                if (wasUp) {
                    reconnects++
                    onDown()
                }
            }
            sleep(backoff)
            backoff = minOf(backoff * 2, MAX_BACKOFF_MS)
        }
        state = "down"
    }

    private fun serve(s: Socket) {
        val writer = Thread({
            try {
                val os = s.getOutputStream()
                while (running && !s.isClosed) {
                    val f = out.poll(250, TimeUnit.MILLISECONDS) ?: continue
                    os.write(f)
                    os.flush()
                }
            } catch (_: Exception) {
                try { s.close() } catch (_: IOException) { }
            }
        }, "glasses-link-tx").also { it.isDaemon = true; it.start() }
        try {
            val ins = s.getInputStream()
            val buf = ByteArray(8192)
            while (running) {
                val n = ins.read(buf)
                if (n < 0) break
                onBytes(buf.copyOf(n))
            }
        } finally {
            try { s.close() } catch (_: IOException) { }
            writer.join(500)
        }
    }

    private fun sleep(ms: Long) {
        try { Thread.sleep(ms) } catch (_: InterruptedException) { }
    }

    companion object {
        private const val TAG = "WeftGlasses"
        private const val MAX_QUEUE = 256
        private const val CONNECT_TIMEOUT_MS = 3000
        private const val MAX_BACKOFF_MS = 3000L
    }
}
