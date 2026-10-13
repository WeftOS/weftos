package ai.weftos.phonenode

import android.Manifest
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import uniffi.weftos_node_core.IndicatorInput

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        NodeRuntime.init(this)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(Modifier.fillMaxSize()) { Home(::startLink, ::stopLink) }
            }
        }
    }

    private fun startLink() = startForegroundService(Intent(this, LinkService::class.java))

    private fun stopLink() {
        startService(Intent(this, LinkService::class.java).setAction(LinkService.ACTION_STOP))
    }
}

@Composable
private fun Home(onStart: () -> Unit, onStop: () -> Unit) {
    val s by NodeRuntime.state.collectAsState()
    val notif = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { onStart() }
    Column(
        Modifier.verticalScroll(rememberScrollState()).padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Weft phone node (P0)", style = MaterialTheme.typography.headlineSmall)
        if (!s.coreLoaded) {
            Text("Rust core not loaded: ${s.coreError ?: "unknown"}", color = MaterialTheme.colorScheme.error)
        }
        Row { Text("Node id: "); Text(s.nodeId.ifEmpty { "-" }) }
        Row { Text("Pairing: "); Text(s.pairing) }
        Row { Text("Indicator: "); Text(s.indicator) }
        Row { Text("Link service: "); Text(if (s.serviceRunning) "running (stub)" else "stopped") }

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = {
                if (Build.VERSION.SDK_INT >= 33) notif.launch(Manifest.permission.POST_NOTIFICATIONS) else onStart()
            }) { Text("Start link") }
            OutlinedButton(onClick = onStop) { Text("Stop") }
        }
        Button(onClick = { NodeRuntime.beginPairing() }) { Text("Begin pairing") }

        Text("Indicator machine (no glasses yet)", style = MaterialTheme.typography.titleSmall)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = { NodeRuntime.feed(IndicatorInput.GateOpen) }) { Text("Gate open") }
            OutlinedButton(onClick = { NodeRuntime.feed(IndicatorInput.GateClose) }) { Text("Gate close") }
            OutlinedButton(onClick = { NodeRuntime.tick() }) { Text("Tick") }
        }
        Text("Stubbed in P0: glasses link, BLE, audio relay, mesh gateway. See README.")
    }
}
