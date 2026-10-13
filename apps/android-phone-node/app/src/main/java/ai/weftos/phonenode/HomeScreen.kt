package ai.weftos.phonenode

import android.content.ClipboardManager
import android.content.Context
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode

@Composable
fun HomeScreen(onStart: () -> Unit, onStop: () -> Unit) {
    val s by NodeRuntime.state.collectAsState()
    Column(
        Modifier.verticalScroll(rememberScrollState()).padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text("Weft phone node (P0)", style = MaterialTheme.typography.headlineSmall)
        if (!s.coreLoaded) {
            Text("Rust core not loaded: ${s.coreError ?: "unknown"}", color = MaterialTheme.colorScheme.error)
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = onStart) { Text("Start link") }
            OutlinedButton(onClick = onStop) { Text("Stop") }
        }
        StatusCard(s)
        PairingCard(s)
        SettingsCard()
        Text("Stubbed in P0: BLE, Noise channel, mesh gateway, on-phone STT. See README.", style = MaterialTheme.typography.bodySmall)
    }
}

@Composable
private fun Line(label: String, value: String) {
    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        Text("$label:", style = MaterialTheme.typography.labelLarge)
        Text(value.ifEmpty { "-" })
    }
}

@Composable
private fun StatusCard(s: NodeUiState) {
    Text("Status", style = MaterialTheme.typography.titleMedium)
    Line("Node id", s.nodeId)
    Line("Service", if (s.serviceRunning) "running" else "stopped")
    Line("Link", s.linkState + if (s.listenPort > 0) " on :${s.listenPort}" else "")
    Line("Glasses", s.peer)
    Line("Glasses indicator", s.indicator)
    Line("Last turn", s.lastTurn)
    if (s.glassesBattery >= 0) Line("Glasses battery", "${s.glassesBattery}%  Wi-Fi ${s.glassesRssi} dBm")
    Line("Transcript", s.transcript)
    Line("Reply", s.replyText)
    Line("Request round trip", if (s.rttMs >= 0) "${s.rttMs} ms" else "")
    Line("Server timings", s.timings)
    Line("Reply audio", if (s.replyAudioMs >= 0) "${s.replyAudioMs} ms" else "")
    if (s.lastError.isNotEmpty()) Text("Error: ${s.lastError}", color = MaterialTheme.colorScheme.error)
    Line("Phone addresses", "")
    Text(s.addresses.ifEmpty { "-" }, style = MaterialTheme.typography.bodySmall)
}

@Composable
private fun PairingCard(s: NodeUiState) {
    Text("Pairing", style = MaterialTheme.typography.titleMedium)
    Line("State", s.pairing)
    val code = s.pairingCode
    if (code != null) {
        Text("Code: $code", style = MaterialTheme.typography.headlineMedium)
        Text("Glasses key ${s.pairingKey}...  Compare with the code on the glasses.", style = MaterialTheme.typography.bodySmall)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { NodeRuntime.confirmPairing() }) { Text("Codes match") }
            OutlinedButton(onClick = { NodeRuntime.rejectPairing() }) { Text("They differ") }
        }
    }
}

@Composable
private fun SettingsCard() {
    val ctx = LocalContext.current
    val store = remember { SettingsStore(ctx) }
    val tokens = remember { keystoreTokenStore(ctx) }
    val initial = remember { store.load() }
    var endpoint by remember { mutableStateOf(initial.endpoint) }
    var port by remember { mutableStateOf(initial.port.toString()) }
    var token by remember { mutableStateOf("") }
    var message by remember { mutableStateOf("") }
    val tokenState by NodeRuntime.state.collectAsState()

    fun saveToken(raw: String?) {
        val t = TokenFormat.normalize(raw)
        if (t == null) {
            message = "That does not look like a token."
        } else {
            tokens.set(t)
            token = ""
            NodeRuntime.refreshStatic(ctx)
            message = "Token saved."
        }
    }

    Text("Settings", style = MaterialTheme.typography.titleMedium)
    OutlinedTextField(
        value = endpoint,
        onValueChange = { endpoint = it },
        label = { Text("Speech endpoint") },
        singleLine = true,
        modifier = Modifier.fillMaxWidth(),
    )
    OutlinedTextField(
        value = port,
        onValueChange = { port = it },
        label = { Text("Glasses listen port") },
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        modifier = Modifier.fillMaxWidth(),
    )
    Button(onClick = {
        val problem = SettingsCheck.endpointProblem(endpoint)?.let { "Endpoint: $it" }
            ?: SettingsCheck.portProblem(port)?.let { "Port: $it" }
        if (problem != null) {
            message = problem
        } else {
            store.save(PhoneSettings(endpoint.trim(), port.trim().toInt()))
            message = "Saved. Stop and start the link to apply the port."
        }
    }) { Text("Save settings") }

    Line("Node token", tokenState.tokenState)
    OutlinedTextField(
        value = token,
        onValueChange = { token = it },
        label = { Text("Paste node token") },
        singleLine = true,
        visualTransformation = PasswordVisualTransformation(),
        modifier = Modifier.fillMaxWidth(),
    )
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Button(onClick = { saveToken(token) }) { Text("Save token") }
        OutlinedButton(onClick = {
            val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            saveToken(cm.primaryClip?.getItemAt(0)?.text?.toString())
        }) { Text("Use clipboard") }
    }
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        OutlinedButton(onClick = {
            val opts = GmsBarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build()
            GmsBarcodeScanning.getClient(ctx, opts).startScan()
                .addOnSuccessListener { saveToken(it.rawValue) }
                .addOnFailureListener { message = "Scan unavailable: ${it.javaClass.simpleName}" }
        }) { Text("Scan QR") }
        OutlinedButton(onClick = {
            tokens.clear()
            NodeRuntime.refreshStatic(ctx)
            message = "Token cleared."
        }) { Text("Clear token") }
    }
    if (message.isNotEmpty()) Text(message, style = MaterialTheme.typography.bodySmall)
}
