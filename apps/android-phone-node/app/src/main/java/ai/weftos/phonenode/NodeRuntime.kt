package ai.weftos.phonenode

import android.content.Context
import android.os.SystemClock
import java.io.File
import java.net.Inet4Address
import java.net.NetworkInterface
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import uniffi.weftos_node_core.PhoneLink

/** What the UI shows. Identity and protocol state come from the Rust core; nothing secret is here. */
data class NodeUiState(
    val coreLoaded: Boolean = false,
    val coreError: String? = null,
    val nodeId: String = "",
    val serviceRunning: Boolean = false,
    val listenPort: Int = 0,
    val linkState: String = "stopped",
    val peer: String = "",
    val pairingCode: String? = null,
    val pairingKey: String = "",
    val pairing: String = "not paired",
    val indicator: String = "idle",
    val lastTurn: String = "",
    val glassesBattery: Int = -1,
    val glassesRssi: Int = 0,
    val transcript: String = "",
    val replyText: String = "",
    val timings: String = "",
    val rttMs: Long = -1,
    val replyAudioMs: Int = -1,
    val lastError: String = "",
    val tokenState: String = "not set",
    val addresses: String = "",
    val cosTokenState: String = "not set",
    val cosLink: String = "off",
    val cosCard: CosCard? = null,
    val cosFields: GlanceFields? = null,
    val cosArt: ByteArray? = null,
    val cosArtCaption: String = "",
    val cosMessage: String = "",
)

/**
 * Owns the single [PhoneLink] (identity and the glasses-link session, both in Rust) and the
 * shared UI state. If the native library is missing the UI shows an error; there is no Kotlin
 * fallback identity, which would mint a second node id.
 */
object NodeRuntime {
    private var link: PhoneLink? = null
    private var engine: LinkEngine? = null
    private var cos: CosLink? = null
    private val _state = MutableStateFlow(NodeUiState())
    val state: StateFlow<NodeUiState> = _state.asStateFlow()

    fun nowMs(): ULong = SystemClock.elapsedRealtime().toULong()

    fun update(f: NodeUiState.() -> NodeUiState) = _state.update { it.f() }

    @Synchronized
    fun init(context: Context) {
        if (link != null) return
        val app = context.applicationContext
        try {
            val dir = File(app.filesDir, "weft-node").also { it.mkdirs() }
            val settings = SettingsStore(app)
            link = PhoneLink(dir.absolutePath, settings.trustedGlasses, System.currentTimeMillis().toULong())
            update { copy(coreLoaded = true, coreError = null, nodeId = link!!.nodeId()) }
        } catch (t: Throwable) { // UnsatisfiedLinkError, or a core error
            update { copy(coreLoaded = false, coreError = t.message ?: t.javaClass.simpleName) }
        }
        refreshStatic(app)
    }

    /** Token state and the phone's own addresses (the hotspot gateway the glasses are told). */
    fun refreshStatic(context: Context) {
        val app = context.applicationContext
        val tokens = keystoreTokenStore(app)
        update {
            copy(
                tokenState = TokenFormat.describe(tokens.get()),
                cosTokenState = TokenFormat.describe(cosTokenStore(app).get()),
                addresses = localAddresses(),
            )
        }
    }

    @Synchronized
    fun startEngine(context: Context) {
        val l = link ?: return
        if (engine != null) return
        val app = context.applicationContext
        val e = LinkEngine(l, SettingsStore(app), keystoreTokenStore(app))
        engine = e
        e.start()
    }

    @Synchronized
    fun stopEngine() {
        engine?.stop()
        engine = null
        update { copy(linkState = "stopped", peer = "", listenPort = 0, indicator = "idle") }
    }

    /** The CoS card link rides in [LinkService] beside the engine; off without a base URL or token. */
    @Synchronized
    fun startCos(context: Context) {
        if (cos != null) return
        val app = context.applicationContext
        val base = SettingsStore(app).load().cosBase
        val token = cosTokenStore(app).get()
        when {
            base.isEmpty() -> update { copy(cosLink = "off (no CoS URL)") }
            token == null -> update { copy(cosLink = "off (no CoS token)") }
            else -> cos = CosLink(app, base, token).also { it.start() }
        }
    }

    @Synchronized
    fun stopCos() {
        cos?.stop()
        cos = null
    }

    fun answerCard(cardId: String, value: String) {
        val c = cos
        if (c == null) update { copy(cosMessage = "Start the link to answer.") } else c.answer(cardId, value)
    }

    fun confirmPairing() = engine?.confirmPairing()

    fun rejectPairing() = engine?.rejectPairing()

    fun setServiceRunning(running: Boolean) = update { copy(serviceRunning = running) }

    private fun localAddresses(): String = try {
        NetworkInterface.getNetworkInterfaces().toList()
            .filter { it.isUp && !it.isLoopback }
            .flatMap { ni ->
                ni.inetAddresses.toList().filterIsInstance<Inet4Address>().map { "${ni.name} ${it.hostAddress}" }
            }
            .joinToString("\n")
    } catch (_: Exception) {
        ""
    }
}
