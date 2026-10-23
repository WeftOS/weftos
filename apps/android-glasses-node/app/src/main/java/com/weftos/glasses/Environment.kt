package com.weftos.glasses

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.ConnectivityManager
import android.net.wifi.WifiManager
import android.os.BatteryManager
import java.net.Inet4Address

/** Reads of device state that status reports (battery, Wi-Fi) and the discovery stub (gateway). */
class Environment(private val ctx: Context) {
    data class Battery(val percent: Int, val charging: Boolean)
    data class Wifi(val rssi: Int, val rateMbps: Int)

    fun battery(): Battery {
        val i: Intent? = ctx.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val level = i?.getIntExtra(BatteryManager.EXTRA_LEVEL, -1) ?: -1
        val scale = i?.getIntExtra(BatteryManager.EXTRA_SCALE, 100) ?: 100
        val status = i?.getIntExtra(BatteryManager.EXTRA_STATUS, -1) ?: -1
        val charging = status == BatteryManager.BATTERY_STATUS_CHARGING || status == BatteryManager.BATTERY_STATUS_FULL
        val pct = if (level >= 0 && scale > 0) level * 100 / scale else 0
        return Battery(pct.coerceIn(0, 100), charging)
    }

    @Suppress("DEPRECATION")
    fun wifi(): Wifi = try {
        val info = ctx.getSystemService(WifiManager::class.java)?.connectionInfo
        Wifi(info?.rssi ?: 0, info?.linkSpeed ?: 0)
    } catch (_: Exception) {
        Wifi(0, 0)
    }

    /**
     * Discovery stub for P0: the Wi-Fi default gateway. On the phone's hotspot the phone is
     * the gateway, so no address is configured or baked in. Real discovery is P1.
     */
    fun gateway(): String? = try {
        val cm = ctx.getSystemService(ConnectivityManager::class.java)
        val net = cm?.activeNetwork
        cm?.getLinkProperties(net)?.routes
            ?.firstOrNull { it.isDefaultRoute && it.gateway is Inet4Address }
            ?.gateway?.hostAddress
    } catch (_: Exception) {
        null
    }
}
