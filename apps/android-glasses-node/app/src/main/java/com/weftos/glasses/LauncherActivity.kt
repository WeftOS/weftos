package com.weftos.glasses

import android.app.Activity
import android.content.Intent
import android.os.Bundle

/**
 * Headless translucent activity: launching it once from the shell brings the process up from
 * a foreground state (the mentramic provisioning trick), then it starts the service and
 * finishes. Normal operation does not need it; see README for the shell-started service.
 */
class LauncherActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        startForegroundService(Intent(this, GlassesService::class.java))
        finish()
    }
}
