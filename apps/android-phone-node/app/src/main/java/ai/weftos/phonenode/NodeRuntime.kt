package ai.weftos.phonenode

import android.content.Context
import android.os.SystemClock
import java.io.File
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import uniffi.weftos_node_core.IndicatorInput
import uniffi.weftos_node_core.IndicatorState
import uniffi.weftos_node_core.NodeCore
import uniffi.weftos_node_core.PairingState

/** What the UI shows. All of it comes from the Rust core; Kotlin derives nothing itself. */
data class NodeUiState(
    val coreLoaded: Boolean = false,
    val coreError: String? = null,
    val nodeId: String = "",
    val pairing: String = "idle",
    val indicator: String = "idle",
    val serviceRunning: Boolean = false,
)

/**
 * Owns the single [NodeCore]. The node id and every key operation live in Rust
 * (weftos-node-core); if the native library is missing we show an error, we do not
 * fall back to a Kotlin identity (that would mint a second, different node id).
 */
object NodeRuntime {
    private var core: NodeCore? = null
    private val _state = MutableStateFlow(NodeUiState())
    val state: StateFlow<NodeUiState> = _state.asStateFlow()

    fun nowMs(): ULong = SystemClock.elapsedRealtime().toULong()

    @Synchronized
    fun init(context: Context) {
        if (core != null) return
        try {
            val dir = File(context.filesDir, "weft-node").also { it.mkdirs() }
            core = NodeCore(dir.absolutePath)
            _state.value = _state.value.copy(coreLoaded = true, coreError = null)
            refresh()
        } catch (t: Throwable) { // UnsatisfiedLinkError, or a core error
            _state.value = _state.value.copy(coreLoaded = false, coreError = t.message ?: t.javaClass.simpleName)
        }
    }

    fun setServiceRunning(running: Boolean) {
        _state.value = _state.value.copy(serviceRunning = running)
    }

    fun beginPairing() {
        core?.beginPairing(null)
        refresh()
    }

    /** Exercises the indicator machine from the UI (no glasses in P0). */
    fun feed(input: IndicatorInput) {
        core?.indicatorFeed(nowMs(), input)
        refresh()
    }

    fun tick() = feed(IndicatorInput.Tick)

    private fun refresh() {
        val c = core ?: return
        _state.value = _state.value.copy(
            nodeId = c.nodeId(),
            pairing = describe(c.pairingState()),
            indicator = describe(c.indicatorState()),
        )
    }

    private fun describe(p: PairingState): String = when (p) {
        is PairingState.Idle -> "idle"
        is PairingState.AwaitingPeer -> "waiting for glasses"
        is PairingState.AwaitingConfirm -> "enter the code the glasses speak (${p.sas})"
        is PairingState.Paired -> "paired"
        is PairingState.Refused -> "refused: ${p.reason}"
    }

    private fun describe(s: IndicatorState): String = s.name.lowercase()
}
