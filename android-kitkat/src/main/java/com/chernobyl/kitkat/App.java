package com.chernobyl.kitkat;

import android.app.Application;
import android.app.Notification;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.widget.Toast;
import java.io.IOException;
import java.lang.reflect.Method;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.URLEncoder;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLPeerUnverifiedException;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;
import java.io.InputStream;
import java.security.KeyStore;
import java.security.cert.CertificateException;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import org.json.JSONObject;

/**
 * Process-wide state: the stored pairing, the one E2E transport, the Model it
 * feeds, live transcript subscriptions, workspace/git request plumbing and the
 * approval notifications. Everything UI-facing runs on the main thread.
 */
public final class App extends Application {
    static final String TAG = "chernobyl";
    static App app;

    final Handler main = new Handler(Looper.getMainLooper());
    final Model model = new Model();
    SecureStore store;
    Transport transport;
    E2e.Pairing pairing;
    String status = Transport.OFFLINE;
    /** Activities showing: notifications are only posted while none is. */
    int visible;

    interface Change { void changed(); }

    private final List<Change> watchers = new ArrayList<Change>();
    private boolean changePosted;
    private final Set<String> strays = new HashSet<String>();
    private final Map<String, Long> lastSub = new HashMap<String, Long>();
    private final Set<String> resubPending = new HashSet<String>();
    private final Map<String, JSONObject[]> wsResults = new HashMap<String, JSONObject[]>();
    private final Map<String, Result> cmdResults = new HashMap<String, Result>();

    @Override
    public void onCreate() {
        super.onCreate();
        app = this;
        store = new SecureStore(this);
        start();
    }

    /* ---------------- pairing + transport ---------------- */

    /** Pair with a payload line and token; throws on an invalid payload. */
    void pair(String payload, String token) throws E2e.E2eException {
        E2e.parsePairing(payload); // validate before replacing anything
        store.put("pairing", payload.trim().startsWith("payload:") ? payload.trim().substring(8).trim() : payload.trim());
        store.put("token", token == null || token.trim().isEmpty() ? null : token.trim());
        store.put("psk_override", null);
        start();
    }

    /** New access token for the stored pairing (the companion refused the old one with 4001). */
    void setToken(String token) {
        store.put("token", token == null || token.trim().isEmpty() ? null : token.trim());
        start();
    }

    void unpair() {
        stopTransport();
        store.put("pairing", null);
        store.put("token", null);
        store.put("psk_override", null);
        pairing = null;
        model.clear();
        changed();
    }

    private void stopTransport() {
        if (transport != null) transport.dispose();
        transport = null;
        status = Transport.OFFLINE;
        ConnService.stop(this);
    }

    void start() {
        stopTransport();
        model.clear();
        String raw = store.get("pairing");
        if (raw == null) { pairing = null; changed(); return; }
        try {
            pairing = E2e.parsePairing(raw);
        } catch (E2e.E2eException e) {
            toast("Stored pairing is invalid: " + e.getMessage());
            pairing = null;
            return;
        }
        String rotated = store.get("psk_override");
        if (rotated != null && Crypto.b64decode(rotated) != null) pairing = pairing.withPsk(Crypto.b64decode(rotated));
        String token = store.get("token");
        List<String> eps = new ArrayList<String>();
        for (String e : pairing.endpoints) {
            try {
                eps.add(token == null ? e : e + (e.contains("?") ? "&" : "?") + "token=" + URLEncoder.encode(token, "UTF-8"));
            } catch (Exception ignored) {
                eps.add(e);
            }
        }
        transport = new Transport(eps, pairing, new Transport.Listener() {
            @Override
            public void onStatus(final String s) {
                main.post(new Runnable() { @Override public void run() { onTransportStatus(s); } });
            }

            @Override
            public void onMessage(final JSONObject m) {
                main.post(new Runnable() { @Override public void run() { onServerMessage(m); } });
            }

            @Override
            public boolean onRekey(String pskB64) {
                // ADR-011 addendum: on disk before the transport acks it
                return store.put("psk_override", pskB64);
            }
        }, OPENER);
        transport.connect();
        ConnService.start(this);
        changed();
    }

    void onTransportStatus(String s) {
        status = s;
        if (Transport.CONNECTED.equals(s)) {
            for (String id : new ArrayList<String>(model.views.keySet())) subscribe(id);
        } else {
            model.synced = false;
        }
        ConnService.update(this);
        changed();
    }

    String statusLabel() {
        if (pairing == null) return "Not paired";
        switch (status) {
            case Transport.CONNECTED: return model.synced ? "Connected" : "Connected, loading…";
            case Transport.CONNECTING: return "Connecting…";
            case Transport.HANDSHAKING: return "Authorizing…";
            case Transport.RECONNECTING: return "Reconnecting…";
            case Transport.DENIED: return "Refused: " + (transport != null ? transport.deniedReason() : "");
            default: return "Offline";
        }
    }

    boolean connected() {
        return Transport.CONNECTED.equals(status);
    }

    /* ---------------- messages ---------------- */

    void onServerMessage(JSONObject m) {
        String type = m.optString("type");
        if (type.equals("transcript") || type.equals("transcript_snapshot")) {
            String sid = m.optString("sessionId");
            Model.View v = model.views.get(sid);
            if (v == null) {
                if (strays.add(sid)) sendQuiet(new JSONObject(map("type", "session_unsubscribe", "commandId", Transport.nextId("sub"), "sessionId", sid)));
                return;
            }
            if (Model.applyTranscript(v, m)) resubscribeSoon(sid);
            changed();
            return;
        }
        if (type.equals("workspace_result")) {
            JSONObject[] slot = wsResults.get(m.optString("commandId"));
            if (slot != null) slot[0] = m.optJSONObject("result");
            return;
        }
        if (type.equals("command_result")) {
            Result r = cmdResults.remove(m.optString("commandId"));
            if (r != null) r.done(m, "completed".equals(m.optString("status")) ? null : m.optString("error", m.optString("status")));
        }
        if (model.apply(m)) {
            if (type.equals("permission_requested")) notifyApproval(m.optJSONObject("permission"));
            if (type.equals("permission_resolved") && m.optJSONObject("permission") != null) cancelNotification(m.optJSONObject("permission").optString("id"));
            changed();
        }
    }

    void watch(Change c) { watchers.add(c); }

    void unwatch(Change c) { watchers.remove(c); }

    /** Coalesced UI refresh: at most one per 120 ms while text streams. */
    void changed() {
        if (changePosted) return;
        changePosted = true;
        main.postDelayed(new Runnable() {
            @Override
            public void run() {
                changePosted = false;
                for (Change c : new ArrayList<Change>(watchers)) c.changed();
            }
        }, 120);
    }

    /* ---------------- commands ---------------- */

    interface Result { void done(JSONObject reply, String error); }

    static Map<String, Object> map(Object... kv) {
        Map<String, Object> m = new HashMap<String, Object>();
        for (int i = 0; i < kv.length; i += 2) if (kv[i + 1] != null) m.put((String) kv[i], kv[i + 1]);
        return m;
    }

    static JSONObject msg(String type, Object... kv) {
        JSONObject o = new JSONObject(map(kv));
        try {
            o.put("type", type);
            o.put("commandId", Transport.nextId(type.startsWith("session_") ? "sub" : "cmd"));
        } catch (Exception ignored) { }
        return o;
    }

    /** Ack callback on the main thread. */
    void send(JSONObject m, final Result r) {
        if (transport == null) { r.done(null, "offline"); return; }
        transport.send(m, new Transport.AckCallback() {
            @Override
            public void done(final JSONObject ack, final String error) {
                main.post(new Runnable() { @Override public void run() { r.done(ack, error); } });
            }
        });
    }

    private void sendQuiet(JSONObject m) {
        send(m, new Result() { @Override public void done(JSONObject reply, String error) { } });
    }

    /**
     * A user command: ack errors toast; "offline" means nothing was sent; a lost
     * ack is an UNKNOWN outcome, never claimed as failed (mandate section 9).
     */
    void command(JSONObject m, final String okToast, final Result r) {
        send(m, new Result() {
            @Override
            public void done(JSONObject ack, String error) {
                if (ack != null && !ack.optBoolean("ok")) toast(ack.optString("error", "Command rejected"));
                else if (ack != null && okToast != null) toast(okToast);
                else if ("offline".equals(error)) toast("Host unreachable — nothing was sent");
                else if (error != null) toast("Outcome unknown — the host may have run it");
                if (r != null) r.done(ack, error);
            }
        });
    }

    /** ADR-016 read: its workspace_result arrives right before the ack. */
    void query(final JSONObject m, final Result r) {
        final String id = m.optString("commandId");
        final JSONObject[] slot = new JSONObject[1];
        wsResults.put(id, slot);
        send(m, new Result() {
            @Override
            public void done(JSONObject ack, String error) {
                wsResults.remove(id);
                if (ack == null) r.done(null, error);
                else if (!ack.optBoolean("ok")) r.done(null, ack.optString("error", "refused by the companion"));
                else if (slot[0] == null) r.done(null, "the companion sent no result");
                else r.done(slot[0], null);
            }
        });
    }

    /** ADR-016 change: resolves with its one command_result (null reply = no result to show). */
    void change(JSONObject m, final Result r) {
        final String id = m.optString("commandId");
        cmdResults.put(id, r);
        command(m, null, new Result() {
            @Override
            public void done(JSONObject ack, String error) {
                if (ack == null || !ack.optBoolean("ok") || ack.optBoolean("duplicate")) {
                    if (cmdResults.remove(id) != null) r.done(null, ack == null ? error : ack.optString("error", null));
                }
            }
        });
        main.postDelayed(new Runnable() {
            @Override
            public void run() {
                Result left = cmdResults.remove(id);
                if (left != null) left.done(null, "no result yet — the outcome is unknown");
            }
        }, 150_000);
    }

    /* ---------------- live transcript subscriptions (data/live.ts) ---------------- */

    Model.View openView(String sessionId) {
        strays.remove(sessionId);
        Model.View v = model.views.get(sessionId);
        if (v == null) {
            v = new Model.View();
            model.views.put(sessionId, v);
        }
        if (connected()) subscribe(sessionId);
        return v;
    }

    void closeView(String sessionId) {
        if (model.views.remove(sessionId) != null && connected())
            sendQuiet(msg("session_unsubscribe", "sessionId", sessionId));
    }

    private void subscribe(final String sessionId) {
        final Model.View v = model.views.get(sessionId);
        if (v == null) return;
        lastSub.put(sessionId, System.currentTimeMillis());
        JSONObject m = msg("session_subscribe", "sessionId", sessionId);
        try {
            if (!v.epoch.isEmpty()) m.put("epoch", v.epoch).put("afterSeq", v.seq);
        } catch (Exception ignored) { }
        send(m, new Result() {
            @Override
            public void done(JSONObject ack, String error) {
                if (ack != null && model.views.get(sessionId) == v) {
                    v.error = ack.optBoolean("ok") ? null : ack.optString("error", "refused by the companion");
                    changed();
                }
            }
        });
    }

    /** A gap or a new epoch: re-subscribe from the cursor, at most once a second. */
    private void resubscribeSoon(final String sessionId) {
        if (!resubPending.add(sessionId)) return;
        Long last = lastSub.get(sessionId);
        long wait = Math.max(0, (last == null ? 0 : last) + 1000 - System.currentTimeMillis());
        main.postDelayed(new Runnable() {
            @Override
            public void run() {
                resubPending.remove(sessionId);
                subscribe(sessionId);
            }
        }, wait);
    }

    /* ---------------- notifications ---------------- */

    static final int SERVICE_NOTE = 1;

    private int noteId(String permissionId) {
        return 1000 + (permissionId.hashCode() & 0xffffff);
    }

    void notifyApproval(JSONObject p) {
        if (p == null || visible > 0 || !Model.isPending(p, System.currentTimeMillis())) return;
        JSONObject a = model.agent(p.optString("agentId"));
        String who = a != null ? a.optString("name", "Agent") : "Agent";
        Intent open = new Intent(this, MainActivity.class).putExtra("sessionId", p.optString("sessionId"))
                .putExtra("permissionId", p.optString("id")).setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        PendingIntent pi = PendingIntent.getActivity(this, noteId(p.optString("id")), open,
                PendingIntent.FLAG_UPDATE_CURRENT | (android.os.Build.VERSION.SDK_INT >= 23 ? PendingIntent.FLAG_IMMUTABLE : 0));
        Notification.Builder b = new Notification.Builder(this).setSmallIcon(R.drawable.ic_note)
                .setContentTitle("Approval needed")
                .setContentText((p.optString("title").contains(" wants ") ? p.optString("title")
                        : who + " wants to " + p.optString("title").toLowerCase(java.util.Locale.ROOT)) + ": " + p.optString("command"))
                .setContentIntent(pi).setAutoCancel(true).setDefaults(Notification.DEFAULT_ALL)
                .setPriority(Notification.PRIORITY_HIGH);
        if (android.os.Build.VERSION.SDK_INT >= 26) b.setChannelId(ConnService.channel(this, "approvals", "Approvals", NotificationManager.IMPORTANCE_HIGH));
        ((NotificationManager) getSystemService(NOTIFICATION_SERVICE)).notify(noteId(p.optString("id")), b.build());
    }

    void cancelNotification(String permissionId) {
        ((NotificationManager) getSystemService(NOTIFICATION_SERVICE)).cancel(noteId(permissionId));
    }

    void toast(String text) {
        Toast.makeText(this, text, Toast.LENGTH_LONG).show();
    }

    /* ---------------- sockets: TLS 1.2 on KitKat ---------------- */

    /**
     * KitKat supports TLS 1.1/1.2 but leaves them off for client sockets: they are
     * enabled here before the handshake, with the ECDHE AES-GCM suites, SNI, and
     * the platform's hostname check after it (the system trust store decides the
     * certificate; nothing is pinned or skipped).
     */
    static final WebSocket.Opener OPENER = new WebSocket.Opener() {
        @Override
        public Socket open(String host, int port, boolean tls, int timeoutMs) throws IOException {
            Socket raw = new Socket();
            raw.connect(new InetSocketAddress(host, port), timeoutMs);
            if (!tls) return raw;
            try {
                SSLContext ctx = SSLContext.getInstance("TLS");
                ctx.init(null, new TrustManager[] {trust()}, null);
                SSLSocket s = (SSLSocket) ctx.getSocketFactory().createSocket(raw, host, port, true);
                List<String> want = new ArrayList<String>();
                List<String> supported = Arrays.asList(s.getSupportedProtocols());
                for (String p : new String[] {"TLSv1.3", "TLSv1.2"}) if (supported.contains(p)) want.add(p);
                if (want.isEmpty()) throw new IOException("this device has no TLS 1.2");
                s.setEnabledProtocols(want.toArray(new String[0]));
                Set<String> suites = new LinkedHashSet<String>();
                for (String c : s.getSupportedCipherSuites())
                    if ((c.contains("_ECDHE_") && (c.contains("_GCM_") || c.contains("CHACHA20"))) || c.startsWith("TLS_AES_") || c.startsWith("TLS_CHACHA20"))
                        suites.add(c);
                for (String c : s.getEnabledCipherSuites()) if (!c.contains("_RC4_") && !c.contains("_NULL_") && !c.contains("_EXPORT_") && !c.contains("_anon_")) suites.add(c);
                s.setEnabledCipherSuites(suites.toArray(new String[0]));
                try { // SNI: ngrok routes by it
                    Method m = s.getClass().getMethod("setHostname", String.class);
                    m.invoke(s, host);
                } catch (Exception ignored) { }
                s.setSoTimeout(timeoutMs);
                s.startHandshake();
                if (!HttpsURLConnection.getDefaultHostnameVerifier().verify(host, s.getSession())) {
                    s.close();
                    throw new SSLPeerUnverifiedException("certificate does not match " + host);
                }
                Log.i(TAG, "TLS " + s.getSession().getProtocol() + " " + s.getSession().getCipherSuite() + " to " + host);
                return s;
            } catch (IOException e) {
                raw.close();
                throw e;
            } catch (Exception e) {
                raw.close();
                throw new IOException(e.toString());
            }
        }
    };

    private static X509TrustManager trust;

    private static X509TrustManager platformTrust(KeyStore ks) throws Exception {
        TrustManagerFactory f = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        f.init(ks);
        for (TrustManager t : f.getTrustManagers()) if (t instanceof X509TrustManager) return (X509TrustManager) t;
        throw new IllegalStateException("no X509TrustManager");
    }

    /**
     * The system trust store, plus ISRG Root X1/X2 (res/raw, SHA-256 fingerprints in
     * ADR-019): Android 4.4 predates them, and ngrok's *.ngrok-free.app certificates
     * chain to them. A chain is accepted if either store validates it; nothing is
     * ever skipped, and the hostname is still checked after the handshake.
     */
    static synchronized X509TrustManager trust() throws Exception {
        if (trust != null) return trust;
        final X509TrustManager system = platformTrust(null);
        KeyStore extra = KeyStore.getInstance(KeyStore.getDefaultType());
        extra.load(null, null);
        CertificateFactory cf = CertificateFactory.getInstance("X.509");
        int[] roots = {R.raw.isrg_root_x1, R.raw.isrg_root_x2};
        for (int i = 0; i < roots.length; i++) {
            InputStream in = app.getResources().openRawResource(roots[i]);
            try {
                extra.setCertificateEntry("isrg" + i, cf.generateCertificate(in));
            } finally {
                in.close();
            }
        }
        final X509TrustManager bundled = platformTrust(extra);
        trust = new X509TrustManager() {
            @Override
            public void checkClientTrusted(X509Certificate[] chain, String auth) throws CertificateException {
                system.checkClientTrusted(chain, auth);
            }

            @Override
            public void checkServerTrusted(X509Certificate[] chain, String auth) throws CertificateException {
                try {
                    system.checkServerTrusted(chain, auth);
                } catch (CertificateException e) {
                    bundled.checkServerTrusted(chain, auth);
                }
            }

            @Override
            public X509Certificate[] getAcceptedIssuers() {
                return system.getAcceptedIssuers();
            }
        };
        return trust;
    }

    static Context ctx() {
        return app;
    }
}
