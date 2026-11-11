package com.chernobyl.kitkat;

import java.io.IOException;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Random;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import org.json.JSONObject;

/**
 * The E2E transport (port of packages/e2e/src/transport.ts) on blocking threads:
 * hs_* handshake on every connection, then sealed frames only; ordered failover
 * over the QR's endpoint list; equal-jitter exponential backoff; a sealed
 * heartbeat; hs_rekey persisted then acked; hs_err, a server proof mismatch and
 * close codes 4001/4002/4003 are terminal ("denied") until connect() is called.
 * Pending commands are replayed with their ORIGINAL commandId after a reconnect.
 *
 * Threads: one connection loop (connect, handshake, read), one writer (all
 * socket writes, so the UI thread never touches the network) and one timer.
 * Listener calls come from the loop or timer thread.
 */
public final class Transport {
    public static final String APP_ORIGIN = "chernobyl://app";
    public static final int PROTOCOL = 2;

    public interface Listener {
        void onStatus(String status);

        void onMessage(JSONObject msg);

        /** Persist the rotated device key (base64); true only once it is stored. */
        boolean onRekey(String pskB64);
    }

    public interface AckCallback {
        /** `ack` is the server's ack, or null with `error` = offline | timed out | send failed | transport closed. */
        void done(JSONObject ack, String error);
    }

    // statuses (transport.ts TransportStatus)
    public static final String OFFLINE = "offline", CONNECTING = "connecting", HANDSHAKING = "handshaking",
            CONNECTED = "connected", RECONNECTING = "reconnecting", DENIED = "denied";

    long ackTimeoutMs = 10_000, handshakeTimeoutMs = 10_000, retryBaseMs = 1_000, retryMaxMs = 15_000,
            stableMs = 30_000, heartbeatMs = 15_000, pongTimeoutMs = 10_000;

    private final List<String> endpoints;
    private volatile E2e.Pairing pairing;
    private final Listener listener;
    private final WebSocket.Opener opener;
    private final String clientId = "kitkat_" + Long.toString(System.currentTimeMillis(), 36);

    private final Object lock = new Object();
    private final ExecutorService writer = Executors.newSingleThreadExecutor();
    private final ScheduledExecutorService timer = Executors.newSingleThreadScheduledExecutor();
    private final Map<String, Pending> pending = new LinkedHashMap<String, Pending>();
    private final Map<String, ScheduledFuture<?>> pongs = new LinkedHashMap<String, ScheduledFuture<?>>();
    private final Random jitter = new Random();

    private volatile String status = OFFLINE;
    private String deniedReason;
    private boolean stopped = true;
    private Thread loop;
    private WebSocket ws;
    private E2e.Session session;
    private ScheduledFuture<?> heartbeat;
    private int attempts;
    private int endpointIdx;
    private boolean wake;
    private static long idSeq;

    private static final class Pending {
        final JSONObject msg;
        final AckCallback cb;
        final ScheduledFuture<?> timeout;

        Pending(JSONObject msg, AckCallback cb, ScheduledFuture<?> timeout) {
            this.msg = msg;
            this.cb = cb;
            this.timeout = timeout;
        }
    }

    public Transport(List<String> endpoints, E2e.Pairing pairing, Listener listener, WebSocket.Opener opener) {
        if (endpoints.isEmpty()) throw new IllegalArgumentException("no endpoint to dial");
        this.endpoints = new ArrayList<String>(endpoints);
        this.pairing = pairing;
        this.listener = listener;
        this.opener = opener;
    }

    public static synchronized String nextId(String prefix) {
        idSeq++;
        return prefix + "_" + Long.toString(System.currentTimeMillis(), 36) + Long.toString(idSeq, 36);
    }

    /** Bounded exponential backoff with equal jitter: within [min(base,max)/2, max]. */
    static long retryDelayMs(int attempt, long base, long max, double rand) {
        double cap = Math.min(max, base * Math.pow(2, Math.min(attempt, 30)));
        return (long) (cap / 2 + rand * (cap / 2));
    }

    public String status() {
        return status;
    }

    public String deniedReason() {
        return deniedReason;
    }

    private void setStatus(String s) {
        if (status.equals(s)) return;
        status = s;
        listener.onStatus(s);
    }

    public void connect() {
        synchronized (lock) {
            stopped = false;
            if (DENIED.equals(status)) setStatus(OFFLINE);
            endpointIdx = 0;
            attempts = 0;
            wake = true;
            lock.notifyAll(); // skips a backoff sleep in progress
            if (loop == null) {
                loop = new Thread(new Runnable() {
                    @Override
                    public void run() {
                        runLoop();
                    }
                }, "chernobyl-transport");
                loop.start();
            }
        }
    }

    public void close() {
        WebSocket s;
        synchronized (lock) {
            stopped = true;
            s = ws;
            ws = null;
            session = null;
            lock.notifyAll();
        }
        if (s != null) s.close();
        stopHeartbeat();
        failPending("transport closed");
        setStatus(OFFLINE);
    }

    /** Shut the threads down for good (unpair). */
    public void dispose() {
        close();
        writer.shutdownNow();
        timer.shutdownNow();
    }

    private void failPending(String reason) {
        List<Pending> all;
        synchronized (pending) {
            all = new ArrayList<Pending>(pending.values());
            pending.clear();
        }
        for (Pending p : all) {
            p.timeout.cancel(false);
            p.cb.done(null, reason);
        }
    }

    private void deny(String reason) {
        deniedReason = reason;
        setStatus(DENIED);
        failPending(reason);
    }

    private static String terminal(int code) {
        switch (code) {
            case 4001: return "unauthorized (bad or missing token)";
            case 4002: return "protocol version mismatch";
            case 4003: return "device revoked";
            default: return null;
        }
    }

    private void runLoop() {
        while (true) {
            String url;
            synchronized (lock) {
                if (stopped || DENIED.equals(status)) {
                    loop = null;
                    return;
                }
                url = endpoints.get(endpointIdx);
                wake = false;
            }
            setStatus(attempts == 0 ? CONNECTING : RECONNECTING);
            boolean handshakeDone = false;
            long liveSince = 0;
            WebSocket sock = null;
            try {
                sock = WebSocket.connect(url, APP_ORIGIN, opener, (int) handshakeTimeoutMs);
                synchronized (lock) {
                    if (stopped) { sock.close(); continue; }
                    ws = sock;
                }
                setStatus(HANDSHAKING);
                final WebSocket hs = sock;
                E2e.Session s = E2e.handshake(new E2e.Io() {
                    @Override public void send(String text) throws IOException { hs.sendText(text); }
                    @Override public String recv() throws IOException { return hs.readText(); }
                }, pairing);
                sock.setReadTimeout(0); // the heartbeat watches liveness from here
                synchronized (lock) {
                    if (stopped || ws != sock) { sock.close(); continue; }
                    session = s;
                    endpointIdx = 0;
                }
                handshakeDone = true;
                liveSince = System.currentTimeMillis();
                setStatus(CONNECTED);
                write(sock, s, "{\"type\":\"hello\",\"clientId\":\"" + clientId + "\",\"protocol\":" + PROTOCOL + "}");
                List<Pending> replay;
                synchronized (pending) { replay = new ArrayList<Pending>(pending.values()); }
                for (Pending p : replay) write(sock, s, p.msg.toString());
                startHeartbeat(sock, s);
                while (true) handleSealed(sock, s, sock.readText());
            } catch (E2e.E2eException e) {
                if (e.code.equals("handshake-rejected") || e.code.equals("server-proof-mismatch")) deny(e.getMessage());
            } catch (WebSocket.Closed c) {
                String t = terminal(c.code);
                if (t != null) deny(t);
            } catch (IOException ignored) {
                // refused, timed out, TLS failure, dropped: retry below
            } catch (RuntimeException ignored) {
                // a malformed frame must never kill the loop
            } finally {
                if (sock != null) sock.close();
                synchronized (lock) {
                    if (ws == sock) { ws = null; session = null; }
                }
                stopHeartbeat();
            }
            long delay;
            synchronized (lock) {
                if (stopped) { setStatus(OFFLINE); loop = null; return; }
                if (DENIED.equals(status)) { loop = null; return; }
                endpointIdx = handshakeDone ? 0 : (endpointIdx + 1) % endpoints.size();
                if (handshakeDone && System.currentTimeMillis() - liveSince >= stableMs) attempts = 0;
                delay = retryDelayMs(attempts, retryBaseMs, retryMaxMs, jitter.nextDouble());
                attempts++;
            }
            setStatus(RECONNECTING);
            synchronized (lock) {
                long until = System.currentTimeMillis() + delay;
                while (!stopped && !wake) {
                    long left = until - System.currentTimeMillis();
                    if (left <= 0) break;
                    try { lock.wait(left); } catch (InterruptedException e) { return; }
                }
            }
        }
    }

    /** Dial now if sitting in backoff (app back in the foreground, network back). */
    public void nudge() {
        synchronized (lock) {
            if (RECONNECTING.equals(status)) {
                wake = true;
                lock.notifyAll();
            }
        }
    }

    private void handleSealed(WebSocket sock, E2e.Session s, String text) throws IOException {
        String inner;
        try {
            inner = s.open(text);
        } catch (E2e.E2eException e) {
            throw new IOException("sealed-frame violation: " + e.code); // re-handshake with fresh counters
        }
        JSONObject m;
        try {
            m = new JSONObject(inner);
        } catch (Exception e) {
            return;
        }
        String type = m.optString("type");
        if (type.equals("hs_rekey")) {
            rekey(sock, s, m);
            return;
        }
        if (type.equals("pong")) {
            ScheduledFuture<?> f;
            synchronized (pongs) { f = pongs.remove(m.optString("commandId")); }
            if (f != null) f.cancel(false);
            return;
        }
        if (type.equals("ack")) {
            Pending p;
            synchronized (pending) { p = pending.remove(m.optString("commandId")); }
            if (p != null) {
                p.timeout.cancel(false);
                p.cb.done(m, null);
            }
        }
        listener.onMessage(m);
    }

    /** ADR-011 addendum: persist, then ack and use the new key from the next handshake on. */
    private void rekey(WebSocket sock, E2e.Session s, JSONObject m) {
        byte[] psk = m.opt("psk") instanceof String ? Crypto.b64decode(m.optString("psk")) : null;
        if (psk == null || psk.length != 32) return;
        if (!listener.onRekey(Crypto.b64encode(psk))) return;
        pairing = pairing.withPsk(psk);
        write(sock, s, "{\"type\":\"hs_rekey_ack\"}");
    }

    private void write(final WebSocket sock, final E2e.Session s, final String plaintext) {
        try {
            writer.execute(new Runnable() {
                @Override
                public void run() {
                    synchronized (lock) {
                        if (ws != sock || session != s) return; // that connection is gone
                    }
                    try {
                        sock.sendText(s.seal(plaintext));
                    } catch (IOException e) {
                        sock.close();
                    }
                }
            });
        } catch (Exception ignored) {
            // disposed
        }
    }

    private void startHeartbeat(final WebSocket sock, final E2e.Session s) {
        if (heartbeatMs <= 0) return;
        heartbeat = timer.scheduleAtFixedRate(new Runnable() {
            @Override
            public void run() {
                final String id = nextId("ping");
                ScheduledFuture<?> f = timer.schedule(new Runnable() {
                    @Override
                    public void run() {
                        synchronized (pongs) { pongs.remove(id); }
                        sock.close(); // missed pong: the read loop ends and reconnects
                    }
                }, pongTimeoutMs, TimeUnit.MILLISECONDS);
                synchronized (pongs) { pongs.put(id, f); }
                write(sock, s, "{\"type\":\"ping\",\"commandId\":\"" + id + "\"}");
            }
        }, heartbeatMs, heartbeatMs, TimeUnit.MILLISECONDS);
    }

    private void stopHeartbeat() {
        if (heartbeat != null) heartbeat.cancel(false);
        heartbeat = null;
        synchronized (pongs) {
            for (ScheduledFuture<?> f : pongs.values()) f.cancel(false);
            pongs.clear();
        }
    }

    /** Sends a command; the callback gets its ack, or an error (offline = definitely not sent). */
    public void send(final JSONObject msg, final AckCallback cb) {
        final String id = msg.optString("commandId", "_t" + System.currentTimeMillis());
        WebSocket sock;
        E2e.Session s;
        synchronized (lock) {
            sock = ws;
            s = session;
        }
        if (sock == null || s == null || !CONNECTED.equals(status)) {
            cb.done(null, "offline");
            return;
        }
        synchronized (pending) {
            if (pending.containsKey(id)) {
                cb.done(null, "command already in flight");
                return;
            }
            ScheduledFuture<?> t;
            try {
                t = timer.schedule(new Runnable() {
                    @Override
                    public void run() {
                        Pending p;
                        synchronized (pending) { p = pending.remove(id); }
                        if (p != null) p.cb.done(null, "timed out");
                    }
                }, ackTimeoutMs, TimeUnit.MILLISECONDS);
            } catch (Exception e) {
                cb.done(null, "transport closed");
                return;
            }
            pending.put(id, new Pending(msg, cb, t));
        }
        write(sock, s, msg.toString());
    }
}
