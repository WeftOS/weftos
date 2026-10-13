package com.weftos.mentramic;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** Forwards RECORD / LEVEL / STOP broadcasts to the foreground MicService. */
public class MicReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context ctx, Intent in) {
        String action = in.getAction();
        if (action == null) return;
        if (MicService.ACTION_STOP.equals(action)) {
            MicService.requestStop();
            return;
        }
        Intent svc = new Intent(ctx, MicService.class);
        svc.setAction(action);
        svc.putExtra("seconds", in.getIntExtra("seconds", 6));
        String name = in.getStringExtra("name");
        svc.putExtra("name", name == null ? "utt" : name);
        ctx.startForegroundService(svc);
    }
}
