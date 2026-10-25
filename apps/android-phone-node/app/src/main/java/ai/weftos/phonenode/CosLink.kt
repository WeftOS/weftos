package ai.weftos.phonenode

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.util.Log
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * The Android side of the CoS card link: one thread holding the event stream (inside the
 * existing [LinkService], no service of its own), a worker for answers and art, the expiry
 * sweep, and the needs-you notification. State goes to [NodeRuntime]; the token never does.
 */
class CosLink(context: Context, base: String, token: String) {
    private val app = context.applicationContext
    private val client = CosGlanceClient(base, token, UrlConnectionCosHttp())
    private val work = Executors.newSingleThreadExecutor { r -> Thread(r, "cos-work").apply { isDaemon = true } }
    private val sweep = Executors.newSingleThreadScheduledExecutor { r -> Thread(r, "cos-sweep").apply { isDaemon = true } }
    private var thread: Thread? = null
    @Volatile private var artAt: String? = null

    private val listener = object : CosGlanceClient.Listener {
        override fun onLink(state: CosLinkState) {
            val text = when (state) {
                CosLinkState.Connecting -> "connecting"
                CosLinkState.Live -> "live"
                is CosLinkState.Retrying -> "${state.code}, retry in ${state.inMs / 1000} s"
            }
            NodeRuntime.update { copy(cosLink = text) }
        }

        override fun onGlance(glance: Glance) = applyGlance(glance)

        override fun onCard(update: CardUpdate) {
            when (update) {
                is CardUpdate.Show -> show(update.card)
                is CardUpdate.Clear -> {
                    val cur = NodeRuntime.state.value.cosCard
                    if (CosGlanceWire.reduce(cur, update) == null && cur != null) show(null)
                    // Back to idle: the art may have changed while the card was up.
                    work.execute { client.fetchGlance()?.let(::applyGlance) }
                }
            }
        }
    }

    fun start() {
        NodeRuntime.update { copy(cosLink = "connecting", cosMessage = "") }
        thread = Thread({ client.run(listener) }, "cos-events").also { it.isDaemon = true; it.start() }
        sweep.scheduleWithFixedDelay({
            val c = NodeRuntime.state.value.cosCard
            if (c != null && !c.isLive(System.currentTimeMillis())) show(null)
        }, 30, 30, TimeUnit.SECONDS)
    }

    fun stop() {
        client.stop()
        thread?.interrupt()
        work.shutdownNow()
        sweep.shutdownNow()
        CosNotifier.cancel(app)
        NodeRuntime.update { copy(cosLink = "off", cosCard = null) }
    }

    /** Answer a card with one of its options, from the screen or the notification. */
    fun answer(cardId: String, value: String) {
        CosGlanceWire.replyProblem(value)?.let { problem ->
            NodeRuntime.update { copy(cosMessage = problem) }
            return
        }
        NodeRuntime.update { copy(cosMessage = "Sending \"$value\"...") }
        work.execute {
            when (val r = client.answer(cardId, value)) {
                is AnswerResult.Accepted -> {
                    if (NodeRuntime.state.value.cosCard?.id == cardId) show(null)
                    NodeRuntime.update { copy(cosMessage = "Answered: $value") }
                }
                is AnswerResult.Stale -> {
                    // 400: unknown, stale, expired or invalid. The CoS leaves its card up, so the
                    // phone shows whatever the glance says is live now (often the same card).
                    Log.i(TAG, "answer refused (400)")
                    val why = r.message.ifEmpty { "refused" }
                    NodeRuntime.update { copy(cosMessage = "Card already resolved or answer refused: $why") }
                    client.fetchGlance()?.let(::applyGlance)
                    NodeRuntime.state.value.cosCard?.takeIf { it.kind == CardKind.NeedsYou }?.let { CosNotifier.post(app, it) }
                }
                is AnswerResult.Failed -> NodeRuntime.update { copy(cosMessage = "Answer failed: ${r.code}") }
            }
        }
    }

    private fun applyGlance(g: Glance) {
        show(g.card?.takeIf { it.isLive(System.currentTimeMillis()) })
        NodeRuntime.update { copy(cosFields = g.fields, cosArtCaption = g.art?.caption.orEmpty()) }
        val art = g.art
        if (art == null) {
            artAt = null
            NodeRuntime.update { copy(cosArt = null) }
        } else if (art.at != artAt || NodeRuntime.state.value.cosArt == null) {
            artAt = art.at
            val png = client.fetchArt(art)
            NodeRuntime.update { copy(cosArt = png) }
        }
    }

    private fun show(card: CosCard?) {
        val before = NodeRuntime.state.value.cosCard
        NodeRuntime.update { copy(cosCard = card) }
        if (card?.kind == CardKind.NeedsYou) {
            if (before != card) CosNotifier.post(app, card) // a glance refetch of the same card is silent
        } else {
            CosNotifier.cancel(app)
        }
    }

    private companion object {
        const val TAG = "CosLink"
    }
}

/** The needs-you notification: tap opens the app on the card; each option is an action. */
object CosNotifier {
    private const val CHANNEL = "cos_cards"
    private const val NOTIFICATION_ID = 2
    const val EXTRA_CARD_ID = "ai.weftos.phonenode.CARD_ID"
    const val EXTRA_VALUE = "ai.weftos.phonenode.CARD_VALUE"

    fun post(context: Context, card: CosCard) {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Chief of staff cards", NotificationManager.IMPORTANCE_HIGH),
        )
        val open = PendingIntent.getActivity(
            context,
            0,
            Intent(context, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
                .putExtra(EXTRA_CARD_ID, card.id),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val b = Notification.Builder(context, CHANNEL)
            .setContentTitle("${card.kind.label}: ${card.title}")
            .setContentText(card.detail.ifEmpty { card.alert })
            .setStyle(Notification.BigTextStyle().bigText(listOf(card.detail, card.alert).filter { it.isNotEmpty() }.joinToString("\n")))
            .setSmallIcon(R.drawable.ic_launcher)
            .setContentIntent(open)
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_REMINDER)
        card.options.forEachIndexed { i, option ->
            val answer = PendingIntent.getService(
                context,
                10 + i,
                Intent(context, LinkService::class.java)
                    .setAction(LinkService.ACTION_ANSWER)
                    .putExtra(EXTRA_CARD_ID, card.id)
                    .putExtra(EXTRA_VALUE, option),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            b.addAction(Notification.Action.Builder(null, option, answer).build())
        }
        nm.notify(NOTIFICATION_ID, b.build())
    }

    fun cancel(context: Context) {
        (context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager).cancel(NOTIFICATION_ID)
    }
}
