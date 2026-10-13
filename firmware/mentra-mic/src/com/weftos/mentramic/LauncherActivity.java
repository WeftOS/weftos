package com.weftos.mentramic;

import android.app.Activity;
import android.os.Bundle;

/** Headless activity: its only job is to bring the app process up. */
public class LauncherActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        finish();
    }
}
