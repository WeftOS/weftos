package com.weftos.glasses

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import android.util.Log
import uniffi.weftos_node_core.LedPattern

/**
 * The glasses agent's foreground service (types: microphone, connectedDevice).
 *
 * Start it the way the proven mentramic app is started, from the adb shell, which is exempt
 * from the background microphone restriction with the display off on Android 11:
 *
 *     adb shell am start-foreground-service -n com.weftos.glasses/.GlassesService
 *
 * It is exported only behind android.permission.DUMP (held by the shell, not by apps).
 * Further `start-foreground-service` calls with an action drive the running agent (see README).
 */
class GlassesService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Glasses agent", NotificationManager.IMPORTANCE_LOW))
        val n = Notification.Builder(this, CHANNEL)
            .setContentTitle("WeftOS glasses agent")
            .setSmallIcon(R.drawable.ic_stat)
            .setOngoing(true)
            .build()
        startForeground(
            NOTIFICATION_ID, n,
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE or ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
        )

        val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val action = intent?.action
        if (action == ACTION_STOP) {
            stopAgent()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }

        // Config: persisted values, overlaid by any string extras on this intent.
        val persisted = prefs.all.mapNotNull { (k, v) -> (v as? String)?.let { k to it } }.toMap()
        val extras = HashMap<String, String>()
        @Suppress("DEPRECATION")
        intent?.extras?.let { b -> for (k in b.keySet()) b.get(k)?.let { extras[k] = it.toString() } }
        val cfg = GlassesConfig.fromMap(extras, GlassesConfig.fromMap(persisted))
        if (extras.isNotEmpty()) prefs.edit().apply { cfg.toMap().forEach { (k, v) -> putString(k, v) } }.apply()

        val a = agent
        if (a == null) {
            agent = Agent(applicationContext, cfg).also { it.start() }
        } else if (extras.isNotEmpty()) {
            a.updateConfig(cfg)
        }
        agent?.let { handle(it, action, extras) }
        return START_STICKY
    }

    private fun handle(a: Agent, action: String?, x: Map<String, String>) {
        when (action) {
            ACTION_DEMO -> a.demo()
            ACTION_STATUS -> a.publishStatus()
            ACTION_CUE -> CueAsset.byName(x["name"] ?: "")?.let { a.playCue(it) }
            ACTION_LED -> a.ledTest(
                LedPattern(
                    color = a.colorOf(x["color"]?.toIntOrNull() ?: 1),
                    onMs = (x["on_ms"]?.toUIntOrNull() ?: 1000u),
                    offMs = (x["off_ms"]?.toUIntOrNull() ?: 100u),
                    cycles = (x["cycles"]?.toUIntOrNull() ?: 1u),
                    brightness = (x["brightness"]?.toUIntOrNull()?.coerceAtMost(255u) ?: 160u).toUByte(),
                ),
            )
            ACTION_LED_OFF -> a.ledOff()
            else -> Unit
        }
    }

    private fun stopAgent() {
        agent?.stop()
        agent = null
    }

    override fun onDestroy() {
        stopAgent()
        super.onDestroy()
    }

    companion object {
        private const val CHANNEL = "weftos-glasses"
        private const val NOTIFICATION_ID = 7701
        private const val PREFS = "glasses-config"
        const val ACTION_STOP = "com.weftos.glasses.STOP"
        const val ACTION_DEMO = "com.weftos.glasses.DEMO"
        const val ACTION_STATUS = "com.weftos.glasses.STATUS"
        const val ACTION_CUE = "com.weftos.glasses.CUE"
        const val ACTION_LED = "com.weftos.glasses.LED"
        const val ACTION_LED_OFF = "com.weftos.glasses.LED_OFF"

        @Volatile private var agent: Agent? = null
    }
}
