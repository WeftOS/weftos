package ai.weftos.phonenode

import android.util.Log
import java.io.IOException
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit
import uniffi.weftos_node_core.IndicatorState
import uniffi.weftos_node_core.PhoneEvent
import uniffi.weftos_node_core.PhoneLink
import uniffi.weftos_node_core.PhoneOut
import uniffi.weftos_node_core.RelayResult

/**
 * The shell around [PhoneLink]: a TCP listener for one glasses peer, the HTTP relay to the
 * speech endpoint, and the clock. Protocol decisions (framing, pairing, utterance assembly,
 * indicator commands, reply frames) are made in Rust; this class moves bytes, performs the
 * POST and reports state to the UI. It logs events and sizes, never audio, never the token.
 */
class LinkEngine(
    private val core: PhoneLink,
    private val settings: SettingsStore,
    private val tokens: TokenStore,
    private val client: SpeechClient = SpeechClient(),
) {
    private val lock = Any()
    private val relayPool = Executors.newCachedThreadPool { r -> Thread(r, "relay").apply { isDaemon = true } }
    private val sched = Executors.newSingleThreadScheduledExecutor { r -> Thread(r, "link-sched").apply { isDaemon = true } }
    private var tickFuture: ScheduledFuture<*>? = null
    @Volatile private var running = false
    @Volatile private var server: ServerSocket? = null
    private var acceptThread: Thread? = null
    private var conn: Conn? = null

    /** Address of the last peer that completed pairing; kept in memory only. */
    private var pairedAddress: String? = null

    private fun now(): ULong = NodeRuntime.nowMs()

    private fun epoch(): ULong = System.currentTimeMillis().toULong()

    private class Conn(val sock: Socket) {
        val addr: String = (sock.remoteSocketAddress as? InetSocketAddress)?.address?.hostAddress ?: "?"
        val out = LinkedBlockingQueue<ByteArray>(2048)

        fun close() {
            try { sock.close() } catch (_: IOException) { }
        }
    }

    fun start() {
        if (running) return
        val port = settings.load().port
        try {
            server = ServerSocket().apply {
                reuseAddress = true
                bind(InetSocketAddress(port))
            }
        } catch (e: IOException) {
            NodeRuntime.update { copy(linkState = "cannot listen", lastError = "port $port: ${e.message}") }
            return
        }
        running = true
        NodeRuntime.update { copy(linkState = "listening", listenPort = port, peer = "", lastError = "") }
        acceptThread = Thread({ acceptLoop() }, "glasses-accept").also { it.isDaemon = true; it.start() }
        Log.i(TAG, "listening on :$port")
    }

    fun stop() {
        running = false
        try { server?.close() } catch (_: IOException) { }
        synchronized(lock) {
            conn?.close()
            conn = null
        }
        acceptThread?.join(1000)
        tickFuture?.cancel(false)
        sched.shutdownNow()
        relayPool.shutdownNow()
    }

    // ---- owner actions -------------------------------------------------------------

    fun confirmPairing() = synchronized(lock) {
        try {
            handle(core.confirmPairing(epoch()), conn)
        } catch (e: Exception) {
            NodeRuntime.update { copy(lastError = "pairing: ${e.message}") }
        }
    }

    fun rejectPairing() = synchronized(lock) {
        try {
            handle(core.rejectPairing(), conn)
        } catch (e: Exception) {
            NodeRuntime.update { copy(lastError = "pairing: ${e.message}") }
        }
    }

    // ---- sockets -------------------------------------------------------------------

    private fun acceptLoop() {
        while (running) {
            val s = try {
                server?.accept() ?: break
            } catch (_: IOException) {
                break
            }
            Thread({ serve(Conn(s)) }, "glasses-conn").also { it.isDaemon = true; it.start() }
        }
    }

    private fun serve(c: Conn) {
        c.sock.tcpNoDelay = true
        synchronized(lock) {
            conn?.close() // one glasses peer: a new connection replaces the old one
            conn = c
            core.onConnect(c.addr == pairedAddress)
            NodeRuntime.update {
                copy(
                    linkState = "connected",
                    peer = c.addr,
                    indicator = "idle",
                    pairing = if (c.addr == pairedAddress) "paired (resumed)" else "awaiting pairing",
                    pairingCode = null,
                )
            }
        }
        val writer = Thread({ writeLoop(c) }, "glasses-tx").also { it.isDaemon = true; it.start() }
        try {
            val ins = c.sock.getInputStream()
            val buf = ByteArray(8192)
            while (running) {
                val n = ins.read(buf)
                if (n < 0) break
                synchronized(lock) {
                    if (conn !== c) return@serve
                    handle(core.onBytes(buf.copyOf(n), now(), epoch()), c)
                }
            }
        } catch (e: IOException) {
            Log.d(TAG, "link read ended: ${e.message}")
        } catch (e: Exception) { // a core error: the stream is poisoned, drop the connection
            Log.w(TAG, "link protocol error: ${e.message}")
            NodeRuntime.update { copy(lastError = "protocol: ${e.message}") }
        } finally {
            synchronized(lock) {
                if (conn === c) {
                    conn = null
                    try { handle(core.onDisconnect(), null) } catch (_: Exception) { }
                    NodeRuntime.update { copy(linkState = if (running) "listening" else "stopped", peer = "", pairingCode = null) }
                }
            }
            c.close()
            writer.join(300)
        }
    }

    private fun writeLoop(c: Conn) {
        try {
            val os = c.sock.getOutputStream()
            while (!c.sock.isClosed) {
                val f = c.out.poll(250, TimeUnit.MILLISECONDS) ?: continue
                os.write(f)
                os.flush()
            }
        } catch (_: Exception) {
            c.close()
        }
    }

    // ---- core output -----------------------------------------------------------------

    /** Must hold [lock]. Writes frames, applies events to the UI, starts relays, re-arms the tick. */
    private fun handle(out: PhoneOut, c: Conn?) {
        if (c != null) for (f in out.send) c.out.offer(f)
        for (e in out.events) onEvent(e, c)
        rearm()
    }

    private fun onEvent(e: PhoneEvent, c: Conn?) {
        when (e) {
            is PhoneEvent.PairingCode -> NodeRuntime.update {
                copy(pairingCode = e.sas, pairingKey = e.glassesKeyHex.take(12), pairing = "compare the code")
            }
            is PhoneEvent.Paired -> {
                pairedAddress = c?.addr
                settings.trustedGlasses = e.glassesKeyHex
                NodeRuntime.update { copy(pairingCode = null, pairing = "paired") }
            }
            is PhoneEvent.PairingRefused -> NodeRuntime.update {
                copy(pairingCode = null, pairing = "refused: ${e.reason}")
            }
            is PhoneEvent.Unauthorized -> NodeRuntime.update {
                copy(pairing = "glasses connected but not paired: traffic ignored")
            }
            is PhoneEvent.GlassesIndicator -> NodeRuntime.update { copy(indicator = e.state.name.lowercase()) }
            is PhoneEvent.GlassesStatus -> NodeRuntime.update {
                copy(glassesBattery = e.battery.toInt(), glassesRssi = e.wifiRssi.toInt())
            }
            is PhoneEvent.Utterance -> {
                Log.i(TAG, "utterance turn=${e.turnId} ${e.durationMs} ms truncated=${e.truncated}")
                NodeRuntime.update { copy(lastTurn = "utterance ${e.durationMs} ms sent", lastError = "") }
                val started = System.nanoTime()
                relayPool.execute { relay(e.turnId, e.pcm, started) }
            }
            is PhoneEvent.UtteranceDropped -> NodeRuntime.update { copy(lastTurn = "dropped: ${e.reason}") }
            is PhoneEvent.ReplyStarted -> NodeRuntime.update { copy(replyAudioMs = e.audioMs.toInt()) }
            is PhoneEvent.RelayTimedOut -> NodeRuntime.update { copy(lastError = "no answer within 20 s") }
            is PhoneEvent.TurnDone -> NodeRuntime.update {
                copy(lastTurn = "turn ended: ${e.state.label()}")
            }
            is PhoneEvent.TurnAborted -> NodeRuntime.update { copy(lastTurn = "turn aborted") }
        }
    }

    private fun IndicatorState.label() = name.lowercase()

    private fun rearm() {
        tickFuture?.cancel(false)
        val d = core.nextDeadline() ?: return
        val delay = (d.toLong() - now().toLong()).coerceAtLeast(0)
        try {
            tickFuture = sched.schedule({ tick() }, delay, TimeUnit.MILLISECONDS)
        } catch (_: java.util.concurrent.RejectedExecutionException) { }
    }

    private fun tick() = synchronized(lock) {
        try {
            handle(core.tick(now()), conn)
        } catch (e: Exception) {
            Log.w(TAG, "tick: ${e.message}")
        }
    }

    // ---- relay -----------------------------------------------------------------------

    private fun relay(turnId: ULong, pcm: ByteArray, startedNs: Long) {
        val verdict: Verdict
        var rtt = -1L
        val token = tokens.get()
        val problem = Wav.limitProblem(pcm)
        if (token == null) {
            verdict = Verdict.Failed("no_token", "no node token configured")
        } else if (problem != null) {
            verdict = Verdict.Failed("bad_audio", problem)
        } else {
            val spent = (System.nanoTime() - startedNs) / 1_000_000
            val call = client.turn(
                settings.load().endpoint,
                token,
                turnId.toString(),
                Wav.pcm16Mono(pcm),
                RELAY_BUDGET_MS - spent,
            )
            verdict = call.verdict
            rtt = call.rttMs
        }
        val result = when (verdict) {
            is Verdict.Reply -> RelayResult.Reply(turnId, verdict.pcm)
            is Verdict.NoSpeech -> RelayResult.NoSpeech(turnId)
            is Verdict.Failed -> RelayResult.Failed(turnId, verdict.code)
        }
        Log.i(TAG, "relay turn=$turnId -> ${verdict.javaClass.simpleName} rtt=${rtt}ms")
        synchronized(lock) {
            NodeRuntime.update {
                when (verdict) {
                    is Verdict.Reply -> copy(
                        transcript = verdict.transcript,
                        replyText = verdict.replyText,
                        timings = verdict.timingsMs.entries.joinToString("  ") { "${it.key} ${it.value} ms" },
                        rttMs = rtt,
                        lastError = "",
                    )
                    is Verdict.NoSpeech -> copy(transcript = "", replyText = "(no speech heard)", rttMs = rtt, lastError = "")
                    is Verdict.Failed -> copy(lastError = "speech endpoint: ${verdict.code}", rttMs = rtt)
                }
            }
            try {
                handle(core.relayResult(now(), result), conn)
            } catch (e: Exception) {
                Log.w(TAG, "relay result: ${e.message}")
            }
        }
    }

    companion object {
        private const val TAG = "WeftPhone"
        private const val RELAY_BUDGET_MS = 20_000L
    }
}
