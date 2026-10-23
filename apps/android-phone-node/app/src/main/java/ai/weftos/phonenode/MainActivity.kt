package ai.weftos.phonenode

import android.Manifest
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        NodeRuntime.init(this)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(androidx.compose.ui.Modifier.fillMaxSize()) { Root() }
            }
        }
    }

    @Composable
    private fun Root() {
        val notif = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { startLink() }
        HomeScreen(
            onStart = {
                if (Build.VERSION.SDK_INT >= 33) notif.launch(Manifest.permission.POST_NOTIFICATIONS) else startLink()
            },
            onStop = ::stopLink,
        )
    }

    private fun startLink() = startForegroundService(Intent(this, LinkService::class.java))

    private fun stopLink() {
        startService(Intent(this, LinkService::class.java).setAction(LinkService.ACTION_STOP))
    }
}
