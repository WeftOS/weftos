package com.weftos.mentramic;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;

/**
 * Headless activity. Launched bare, it only brings the process up. Launched
 * with a RECORD or LEVEL action, it starts the capture itself: a microphone
 * foreground service started from a visible activity passes Android 14's
 * while-in-use check, which a broadcast from the background may not.
 *
 *   am start -n com.weftos.mentramic/.LauncherActivity \
 *     -a com.weftos.mentramic.RECORD --ei seconds 6 --es name utt1
 */
public class LauncherActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Intent in = getIntent();
        String action = in == null ? null : in.getAction();
        if (MicService.ACTION_STOP.equals(action)) {
            MicService.requestStop();
        } else if (action != null && !Intent.ACTION_MAIN.equals(action)) {
            Intent svc = new Intent(this, MicService.class);
            svc.setAction(action);
            svc.putExtra("seconds", in.getIntExtra("seconds", 6));
            String name = in.getStringExtra("name");
            svc.putExtra("name", name == null ? "utt" : name);
            startForegroundService(svc);
        }
        finish();
    }
}
