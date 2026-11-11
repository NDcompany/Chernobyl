package com.chernobyl.kitkat;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.net.wifi.WifiManager;
import android.os.IBinder;
import android.os.PowerManager;

/**
 * Foreground service (ADR-011 §4): keeps the process, and with it the transport,
 * alive while the app is in the background so approval alerts still arrive.
 * Optional "keep awake" holds a partial wake lock + Wi-Fi lock: without it a
 * sleeping KitKat tablet delivers alerts once it wakes (screen on, or the radio
 * wakes it for the incoming frame), and the companion's 45 s dead-peer window
 * may drop the connection meanwhile; the transport reconnects on wake.
 */
public final class ConnService extends Service {
    private PowerManager.WakeLock wake;
    private WifiManager.WifiLock wifi;

    static void start(Context c) {
        try {
            c.startService(new Intent(c, ConnService.class));
        } catch (Exception ignored) {
            // Android 8+ refuses background starts; KitKat never does
        }
    }

    static void stop(Context c) {
        c.stopService(new Intent(c, ConnService.class));
    }

    static void update(Context c) {
        App a = App.app;
        if (a.transport == null) return;
        ((NotificationManager) c.getSystemService(NOTIFICATION_SERVICE)).notify(App.SERVICE_NOTE, note(c));
    }

    static boolean keepAwake(Context c) {
        return c.getSharedPreferences("settings", MODE_PRIVATE).getBoolean("keepAwake", false);
    }

    static String channel(Context c, String id, String name, int importance) {
        if (android.os.Build.VERSION.SDK_INT >= 26) {
            ((NotificationManager) c.getSystemService(NOTIFICATION_SERVICE)).createNotificationChannel(new NotificationChannel(id, name, importance));
        }
        return id;
    }

    static Notification note(Context c) {
        PendingIntent pi = PendingIntent.getActivity(c, 0, new Intent(c, MainActivity.class),
                android.os.Build.VERSION.SDK_INT >= 23 ? PendingIntent.FLAG_IMMUTABLE : 0);
        String s = App.app.status;
        String text = Transport.CONNECTED.equals(s) ? "Connected to your desktop companion"
                : Transport.DENIED.equals(s) ? "Refused by your desktop companion - open to pair again"
                : Transport.RECONNECTING.equals(s) ? "Reconnecting to your desktop companion…" : "Connecting to your desktop companion…";
        Notification.Builder b = new Notification.Builder(c).setSmallIcon(R.drawable.ic_note).setContentTitle("CHERNOBYL")
                .setContentText(text).setContentIntent(pi).setOngoing(true).setPriority(Notification.PRIORITY_LOW);
        if (android.os.Build.VERSION.SDK_INT >= 26) b.setChannelId(channel(c, "connection", "Background connection", NotificationManager.IMPORTANCE_LOW));
        return b.build();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        startForeground(App.SERVICE_NOTE, note(this));
        applyKeepAwake();
        return START_STICKY;
    }

    void applyKeepAwake() {
        boolean want = keepAwake(this);
        if (want && wake == null) {
            wake = ((PowerManager) getSystemService(POWER_SERVICE)).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "chernobyl:connection");
            wake.acquire();
            wifi = ((WifiManager) getApplicationContext().getSystemService(WIFI_SERVICE)).createWifiLock(WifiManager.WIFI_MODE_FULL, "chernobyl");
            wifi.acquire();
        } else if (!want && wake != null) {
            release();
        }
    }

    private void release() {
        if (wake != null && wake.isHeld()) wake.release();
        if (wifi != null && wifi.isHeld()) wifi.release();
        wake = null;
        wifi = null;
    }

    @Override
    public void onDestroy() {
        release();
        stopForeground(true);
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
