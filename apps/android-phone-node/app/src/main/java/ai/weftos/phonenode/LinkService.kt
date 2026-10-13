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
 * Link-and-relay service stub (design 7.1). Foreground type `connectedDevice`, never
 * `dataSync` (six-hour cap). P0 stub: it holds the foreground notification and the
 * lifecycle only. The TCP listener for the glasses, the upstream stream and BLE land in
 * P1; nothing here opens a socket.
 */
class LinkService : Service() {
    override fun onCreate() {
        super.onCreate()
        NodeRuntime.init(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
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
            .setContentText("Link service running (P0 stub, no glasses link yet)")
            .setSmallIcon(R.drawable.ic_launcher)
            .setOngoing(true)
            .build()
        startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        NodeRuntime.setServiceRunning(true)
        return START_STICKY
    }

    override fun onDestroy() {
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
