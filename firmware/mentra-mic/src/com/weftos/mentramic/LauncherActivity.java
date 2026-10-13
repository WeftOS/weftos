package com.weftos.mentramic;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;

/**
 * Translucent activity. Launched bare, it only brings the process up.
 * Launched with a RECORD or LEVEL action, it starts the capture and stays
 * resumed until recording has begun: Android 11+ grants a microphone
 * foreground service mic access only while the app has a visible activity
 * when the service enters the foreground, so finishing in onCreate made it
 * a background start and AudioRecord.start was refused.
 *
 *   am start -n com.weftos.mentramic/.LauncherActivity \
 *     -a com.weftos.mentramic.RECORD --ei seconds 6 --es name utt1
 */
public class LauncherActivity extends Activity {
    private static final long WAIT_MS = 4000;
    private final Handler handler = new Handler(Looper.getMainLooper());
    private boolean launched = false;
    private long deadline;

    @Override
    protected void onResume() {
        super.onResume();
        if (launched) return;
        launched = true;
        Intent in = getIntent();
        String action = in == null ? null : in.getAction();
        if (MicService.ACTION_STOP.equals(action)) {
            MicService.requestStop();
            finish();
            return;
        }
        if (action == null || Intent.ACTION_MAIN.equals(action)) {
            finish();
            return;
        }
        MicService.started = false;
        Intent svc = new Intent(this, MicService.class);
        svc.setAction(action);
        svc.putExtra("seconds", in.getIntExtra("seconds", 6));
        String name = in.getStringExtra("name");
        svc.putExtra("name", name == null ? "utt" : name);
        startForegroundService(svc);
        deadline = System.currentTimeMillis() + WAIT_MS;
        handler.post(this::waitForStart);
    }

    private void waitForStart() {
        if (MicService.started || System.currentTimeMillis() > deadline) {
            finish();
            overridePendingTransition(0, 0);
        } else {
            handler.postDelayed(this::waitForStart, 50);
        }
    }
}
