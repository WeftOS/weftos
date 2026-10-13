package ai.weftos.phonenode

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder

/**
 * Link-and-relay service (design 7.1). Foreground type `connectedDevice`, never `dataSync`
 * (six-hour cap). It owns the notification and the lifecycle; the glasses listener and the
 * speech relay live in [LinkEngine]. BLE and the mesh gateway are later phases.
 */
class LinkService : Service() {
    override fun onCreate() {
        super.onCreate()
        NodeRuntime.init(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            NodeRuntime.stopEngine()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            NodeRuntime.setServiceRunning(false)
            return START_NOT_STICKY
        }
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Glasses link", NotificationManager.IMPORTANCE_LOW),
        )
        val n = Notification.Builder(this, CHANNEL)
            .setContentTitle("Weft phone node")
            .setContentText("Listening for the glasses and relaying speech")
            .setSmallIcon(R.drawable.ic_launcher)
            .setOngoing(true)
            .build()
        startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        NodeRuntime.setServiceRunning(true)
        NodeRuntime.startEngine(this)
        return START_STICKY
    }

    override fun onDestroy() {
        NodeRuntime.stopEngine()
        NodeRuntime.setServiceRunning(false)
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        const val ACTION_STOP = "ai.weftos.phonenode.STOP"
        private const val CHANNEL = "link"
        private const val NOTIFICATION_ID = 1
    }
}
