package com.weftos.mentramic;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.media.MediaRecorder;
import android.os.IBinder;
import android.util.Log;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;

/** Foreground microphone service: one capture per start command. */
public class MicService extends Service {
    static final String ACTION_RECORD = "com.weftos.mentramic.RECORD";
    static final String ACTION_STOP = "com.weftos.mentramic.STOP";
    static final String ACTION_LEVEL = "com.weftos.mentramic.LEVEL";

    private static final String TAG = "MentraMic";
    private static final String CHANNEL = "mentramic";
    private static final int RATE = 16000;
    private static final int MAX_SECONDS = 30;

    private static volatile boolean stopRequested = false;
    private static volatile boolean busy = false;
    /** Set once a capture is recording or has failed; the launcher waits on it. */
    static volatile boolean started = false;

    static void requestStop() {
        stopRequested = true;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent in, int flags, int startId) {
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(
                new NotificationChannel(CHANNEL, "Mic capture", NotificationManager.IMPORTANCE_LOW));
        Notification n = new Notification.Builder(this, CHANNEL)
                .setContentTitle("Mentra mic capture")
                .setSmallIcon(android.R.drawable.ic_btn_speak_now)
                .build();
        startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);

        final String action = in == null ? null : in.getAction();
        final int seconds = in == null ? 6 : clamp(in.getIntExtra("seconds", 6));
        final String name = in == null ? "utt" : sanitize(in.getStringExtra("name"));
        final int id = startId;

        if (action == null || busy) {
            // Ignore overlapping captures; do not tear down the running one.
            if (!busy) stopSelf(id);
            return START_NOT_STICKY;
        }
        busy = true;
        stopRequested = false;
        new Thread(() -> {
            try {
                if (ACTION_LEVEL.equals(action)) {
                    capture("level", 1, true);
                } else {
                    capture(name, seconds, false);
                }
            } finally {
                started = true;
                busy = false;
                stopForeground(STOP_FOREGROUND_REMOVE);
                stopSelf();
            }
        }, "mentramic-capture").start();
        return START_NOT_STICKY;
    }

    private static int clamp(int s) {
        if (s < 1) return 6;
        return Math.min(s, MAX_SECONDS);
    }

    private static String sanitize(String n) {
        if (n == null || n.isEmpty()) return "utt";
        String s = n.replaceAll("[^A-Za-z0-9_.-]", "_");
        return s.startsWith(".") ? "_" + s : s;
    }

    private AudioRecord open(int source, int bufBytes) {
        try {
            AudioRecord r = new AudioRecord(source, RATE, AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT, bufBytes);
            if (r.getState() == AudioRecord.STATE_INITIALIZED) return r;
            r.release();
        } catch (Exception e) {
            Log.w(TAG, "open source " + source + " failed", e);
        }
        return null;
    }

    private void capture(String name, int seconds, boolean levelOnly) {
        File dir = getExternalFilesDir(null);
        if (dir == null) return;
        AudioRecord rec = null;
        try {
            int min = AudioRecord.getMinBufferSize(RATE, AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT);
            int bufBytes = Math.max(min, RATE * 2 / 4);
            String source = "VOICE_RECOGNITION";
            rec = open(MediaRecorder.AudioSource.VOICE_RECOGNITION, bufBytes);
            if (rec == null) {
                source = "MIC";
                rec = open(MediaRecorder.AudioSource.MIC, bufBytes);
            }
            if (rec == null) throw new IOException("AudioRecord init failed for VOICE_RECOGNITION and MIC");

            int wantBytes = RATE * seconds * 2;
            ByteArrayOutputStream pcm = new ByteArrayOutputStream(wantBytes);
            byte[] buf = new byte[4096];
            rec.startRecording();
            started = true;
            if (rec.getRecordingState() != AudioRecord.RECORDSTATE_RECORDING) {
                throw new IOException("startRecording refused (mic busy or permission missing)");
            }
            while (pcm.size() < wantBytes && !stopRequested) {
                int n = rec.read(buf, 0, Math.min(buf.length, wantBytes - pcm.size()));
                if (n < 0) throw new IOException("AudioRecord.read error " + n);
                pcm.write(buf, 0, n);
            }
            rec.stop();

            byte[] data = pcm.toByteArray();
            int frames = data.length / 2;
            long peak = 0;
            double sumSq = 0;
            for (int i = 0; i < frames; i++) {
                int s = (short) ((data[2 * i] & 0xff) | (data[2 * i + 1] << 8));
                peak = Math.max(peak, Math.abs(s));
                sumSq += (double) s * s;
            }
            double rms = frames == 0 ? 0 : Math.sqrt(sumSq / frames);
            String stats = String.format(java.util.Locale.US,
                    "{\"frames\":%d,\"seconds\":%.3f,\"peak\":%d,\"rms\":%.1f,\"source\":\"%s\"}",
                    frames, frames / (double) RATE, peak, rms, source);

            if (levelOnly) {
                writeAtomic(new File(dir, "level.json"), stats.getBytes(StandardCharsets.UTF_8));
            } else {
                writeAtomic(new File(dir, name + ".wav"), wav(data));
                writeAtomic(new File(dir, name + ".done"), stats.getBytes(StandardCharsets.UTF_8));
            }
        } catch (Throwable t) {
            Log.e(TAG, "capture failed", t);
            try {
                String msg = String.valueOf(t.getMessage()).replace("\\", "\\\\").replace("\"", "'");
                writeAtomic(new File(dir, (levelOnly ? "level" : name) + ".err"),
                        msg.getBytes(StandardCharsets.UTF_8));
            } catch (IOException ignored) {
            }
        } finally {
            if (rec != null) rec.release();
        }
    }

    private static byte[] wav(byte[] pcm) {
        int len = pcm.length;
        byte[] out = new byte[44 + len];
        put(out, 0, "RIFF");
        le32(out, 4, 36 + len);
        put(out, 8, "WAVEfmt ");
        le32(out, 16, 16);
        out[20] = 1; // PCM
        out[22] = 1; // mono
        le32(out, 24, RATE);
        le32(out, 28, RATE * 2);
        out[32] = 2; // block align
        out[34] = 16; // bits
        put(out, 36, "data");
        le32(out, 40, len);
        System.arraycopy(pcm, 0, out, 44, len);
        return out;
    }

    private static void put(byte[] b, int off, String s) {
        for (int i = 0; i < s.length(); i++) b[off + i] = (byte) s.charAt(i);
    }

    private static void le32(byte[] b, int off, int v) {
        b[off] = (byte) v;
        b[off + 1] = (byte) (v >> 8);
        b[off + 2] = (byte) (v >> 16);
        b[off + 3] = (byte) (v >> 24);
    }

    /** Write to a dot-prefixed temp file in the same dir, then rename into place. */
    private static void writeAtomic(File dest, byte[] data) throws IOException {
        File tmp = new File(dest.getParentFile(), "." + dest.getName() + ".tmp");
        try (FileOutputStream fo = new FileOutputStream(tmp)) {
            fo.write(data);
            fo.getFD().sync();
        }
        if (!tmp.renameTo(dest)) {
            tmp.delete();
            throw new IOException("rename failed for " + dest.getName());
        }
    }
}
