package ai.weftos.phonenode

import android.content.ClipboardManager
import android.content.Context
import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
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
import androidx.compose.ui.graphics.FilterQuality
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp

/** The live CoS card with its option buttons, or the idle art and caption when there is none. */
@Composable
fun CosCardView(s: NodeUiState) {
    val card = s.cosCard
    val colors = if (card?.kind == CardKind.NeedsYou) {
        CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer)
    } else {
        CardDefaults.cardColors()
    }
    Card(Modifier.fillMaxWidth(), colors = colors) {
        Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text("Chief of staff", style = MaterialTheme.typography.titleMedium)
            if (card != null) {
                Text(card.kind.label.uppercase(), style = MaterialTheme.typography.labelLarge)
                Text(card.title, style = MaterialTheme.typography.titleLarge)
                if (card.detail.isNotEmpty()) Text(card.detail)
                if (card.alert.isNotEmpty()) Text(card.alert, color = MaterialTheme.colorScheme.error)
                if (card.options.isNotEmpty()) {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        for (o in card.options) Button(onClick = { NodeRuntime.answerCard(card.id, o) }) { Text(o) }
                    }
                } else {
                    FreeReply(card)
                }
            } else {
                IdleArt(s)
            }
            if (s.cosMessage.isNotEmpty()) Text(s.cosMessage, style = MaterialTheme.typography.bodySmall)
            Text("CoS link: ${s.cosLink}", style = MaterialTheme.typography.bodySmall)
        }
    }
}

/** A card with no options takes a short free-text answer. */
@Composable
private fun FreeReply(card: CosCard) {
    var reply by remember(card.id) { mutableStateOf("") }
    OutlinedTextField(
        value = reply,
        onValueChange = { reply = it },
        label = { Text("Reply (up to ${CosGlanceWire.MAX_REPLY} characters)") },
        modifier = Modifier.fillMaxWidth(),
    )
    Button(
        onClick = { NodeRuntime.answerCard(card.id, reply) },
        enabled = CosGlanceWire.replyProblem(reply) == null,
    ) { Text("Send reply") }
}

@Composable
private fun IdleArt(s: NodeUiState) {
    val png = s.cosArt
    val bitmap = remember(png) { png?.let { BitmapFactory.decodeByteArray(it, 0, it.size)?.asImageBitmap() } }
    if (bitmap != null) {
        // 250x122 1-bit panel art: scale up without smoothing so the pixels stay crisp.
        Image(
            bitmap,
            contentDescription = s.cosArtCaption.ifEmpty { "Glance art" },
            modifier = Modifier.fillMaxWidth().aspectRatio(bitmap.width.toFloat() / bitmap.height),
            contentScale = ContentScale.Fit,
            filterQuality = FilterQuality.None,
        )
    }
    if (s.cosArtCaption.isNotEmpty()) Text(s.cosArtCaption, style = MaterialTheme.typography.bodyMedium)
    val f = s.cosFields
    if (f != null && (f.task.isNotEmpty() || f.next.isNotEmpty())) {
        Text(listOf(f.state, f.task).filter { it.isNotEmpty() }.joinToString(": "), style = MaterialTheme.typography.bodySmall)
        if (f.next.isNotEmpty()) Text("Next: ${f.next}", style = MaterialTheme.typography.bodySmall)
    }
    if (bitmap == null && s.cosArtCaption.isEmpty() && f == null) Text("No card.", style = MaterialTheme.typography.bodySmall)
}

/** CoS base URL and endpoint token (`weftos-cos endpoint add`); separate from the speech token. */
@Composable
fun CosSettingsCard() {
    val ctx = LocalContext.current
    val store = remember { SettingsStore(ctx) }
    val tokens = remember { cosTokenStore(ctx) }
    var base by remember { mutableStateOf(store.load().cosBase) }
    var token by remember { mutableStateOf("") }
    var message by remember { mutableStateOf("") }
    val s by NodeRuntime.state.collectAsState()

    fun saveToken(raw: String?) {
        val t = TokenFormat.normalize(raw)
        if (t == null) {
            message = "That does not look like a token."
        } else {
            tokens.set(t)
            token = ""
            NodeRuntime.refreshStatic(ctx)
            message = "CoS token saved. Stop and start the link to apply."
        }
    }

    Text("Chief of staff", style = MaterialTheme.typography.titleMedium)
    OutlinedTextField(
        value = base,
        onValueChange = { base = it },
        label = { Text("CoS URL (https://, or http:// to a .ts.net name)") },
        singleLine = true,
        modifier = Modifier.fillMaxWidth(),
    )
    Button(onClick = {
        val problem = SettingsCheck.cosBaseProblem(base)
        if (problem != null) {
            message = "CoS URL: $problem"
        } else {
            store.save(store.load().copy(cosBase = base.trim().trimEnd('/')))
            message = "Saved. Stop and start the link to apply."
        }
    }) { Text("Save CoS URL") }
    Line("CoS token", s.cosTokenState)
    OutlinedTextField(
        value = token,
        onValueChange = { token = it },
        label = { Text("Paste CoS endpoint token") },
        singleLine = true,
        visualTransformation = PasswordVisualTransformation(),
        modifier = Modifier.fillMaxWidth(),
    )
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Button(onClick = { saveToken(token) }) { Text("Save CoS token") }
        OutlinedButton(onClick = {
            val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            saveToken(cm.primaryClip?.getItemAt(0)?.text?.toString())
        }) { Text("Use clipboard") }
        OutlinedButton(onClick = {
            tokens.clear()
            NodeRuntime.refreshStatic(ctx)
            message = "CoS token cleared."
        }) { Text("Clear") }
    }
    if (message.isNotEmpty()) Text(message, style = MaterialTheme.typography.bodySmall)
}
