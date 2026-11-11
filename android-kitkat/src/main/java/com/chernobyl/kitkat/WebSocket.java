package com.chernobyl.kitkat;

import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.Socket;
import java.net.URI;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.Locale;

/**
 * Minimal RFC 6455 client (text frames, ping/pong, close codes) over a plain or
 * TLS socket: no OkHttp, nothing above API 19. The socket comes from an
 * {@link Opener} so the app can hand in a TLS 1.2 socket (App.openSocket) and
 * JVM tests a plain one.
 */
public final class WebSocket {
    /** The companion's (or the network's) close; `code` is -1 when the socket just ended. */
    public static final class Closed extends IOException {
        public final int code;

        public Closed(int code, String why) {
            super(why);
            this.code = code;
        }
    }

    public interface Opener {
        Socket open(String host, int port, boolean tls, int timeoutMs) throws IOException;
    }

    /** Server frames above this are refused (the companion caps at 256,000 bytes). */
    static final int MAX_FRAME = 4 * 1024 * 1024;

    private final Socket sock;
    private final InputStream in;
    private final OutputStream out;
    private final SecureRandom maskRng = new SecureRandom();
    private boolean closeSent;

    private WebSocket(Socket s) throws IOException {
        sock = s;
        in = new BufferedInputStream(s.getInputStream());
        out = s.getOutputStream();
    }

    /** TCP (+TLS) + HTTP upgrade. `origin` null sends no Origin header. */
    public static WebSocket connect(String url, String origin, Opener opener, int timeoutMs) throws IOException {
        URI u;
        try {
            u = new URI(url);
        } catch (Exception e) {
            throw new IOException("bad url");
        }
        String scheme = u.getScheme() == null ? "" : u.getScheme().toLowerCase(Locale.US);
        boolean tls = scheme.equals("wss");
        if (!tls && !scheme.equals("ws")) throw new IOException("not a ws(s) url");
        String host = u.getHost();
        if (host == null) throw new IOException("url has no host");
        int port = u.getPort() > 0 ? u.getPort() : (tls ? 443 : 80);
        String path = (u.getRawPath() == null || u.getRawPath().isEmpty() ? "/" : u.getRawPath())
                + (u.getRawQuery() != null ? "?" + u.getRawQuery() : "");
        Socket s = opener.open(host, port, tls, timeoutMs);
        boolean ok = false;
        try {
            s.setSoTimeout(timeoutMs);
            WebSocket ws = new WebSocket(s);
            String key = Crypto.b64encode(Crypto.random(16));
            StringBuilder req = new StringBuilder();
            req.append("GET ").append(path).append(" HTTP/1.1\r\n");
            req.append("Host: ").append(host.contains(":") ? "[" + host + "]" : host);
            if (port != (tls ? 443 : 80)) req.append(':').append(port);
            req.append("\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n");
            req.append("Sec-WebSocket-Key: ").append(key).append("\r\nSec-WebSocket-Version: 13\r\n");
            if (origin != null) req.append("Origin: ").append(origin).append("\r\n");
            req.append("\r\n");
            ws.out.write(req.toString().getBytes(Crypto.UTF8));
            ws.out.flush();
            String head = ws.readHead();
            String[] lines = head.split("\r\n");
            if (lines.length == 0 || !lines[0].matches("HTTP/1\\.1 101\\b.*")) {
                throw new IOException("upgrade refused: " + (lines.length > 0 ? lines[0] : "empty response"));
            }
            String expect = Crypto.b64encode(MessageDigest.getInstance("SHA-1")
                    .digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").getBytes(Crypto.UTF8)));
            boolean accepted = false;
            for (String l : lines) {
                int c = l.indexOf(':');
                if (c > 0 && l.substring(0, c).trim().equalsIgnoreCase("sec-websocket-accept")
                        && l.substring(c + 1).trim().equals(expect)) accepted = true;
            }
            if (!accepted) throw new IOException("bad Sec-WebSocket-Accept");
            ok = true;
            return ws;
        } catch (IOException e) {
            throw e;
        } catch (Exception e) {
            throw new IOException(e.toString());
        } finally {
            if (!ok) try { s.close(); } catch (IOException ignored) { }
        }
    }

    private String readHead() throws IOException {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        int state = 0;
        while (state < 4) {
            int c = in.read();
            if (c < 0) throw new EOFException("connection closed during upgrade");
            b.write(c);
            if (b.size() > 16384) throw new IOException("upgrade response too large");
            state = (c == '\r' && (state == 0 || state == 2)) ? state + 1 : (c == '\n' && (state == 1 || state == 3)) ? state + 1 : 0;
        }
        return new String(b.toByteArray(), Crypto.UTF8);
    }

    public void setReadTimeout(int ms) throws IOException {
        sock.setSoTimeout(ms);
    }

    private void readFully(byte[] b) throws IOException {
        int o = 0;
        while (o < b.length) {
            int r = in.read(b, o, b.length - o);
            if (r < 0) throw new Closed(-1, "connection closed");
            o += r;
        }
    }

    /** Next text message; answers pings, skips pongs and binary, throws Closed on a close frame. */
    public String readText() throws IOException {
        ByteArrayOutputStream msg = null;
        boolean binary = false; // binary is not part of the contract: read and dropped
        while (true) {
            byte[] h = new byte[2];
            readFully(h);
            boolean fin = (h[0] & 0x80) != 0;
            int op = h[0] & 0x0f;
            boolean masked = (h[1] & 0x80) != 0;
            long len = h[1] & 0x7f;
            if (len == 126) {
                byte[] e = new byte[2];
                readFully(e);
                len = ((e[0] & 0xff) << 8) | (e[1] & 0xff);
            } else if (len == 127) {
                byte[] e = new byte[8];
                readFully(e);
                len = 0;
                for (byte x : e) len = (len << 8) | (x & 0xff);
            }
            if (len < 0 || len > MAX_FRAME) throw new IOException("frame too large");
            byte[] mask = null;
            if (masked) {
                mask = new byte[4];
                readFully(mask);
            }
            byte[] p = new byte[(int) len];
            readFully(p);
            if (mask != null) for (int i = 0; i < p.length; i++) p[i] ^= mask[i & 3];
            if (op == 0x8) {
                int code = p.length >= 2 ? ((p[0] & 0xff) << 8) | (p[1] & 0xff) : 1005;
                try { sendFrame(0x8, p.length >= 2 ? new byte[] {p[0], p[1]} : new byte[0]); } catch (IOException ignored) { }
                throw new Closed(code, "closed by peer (" + code + ")");
            }
            if (op == 0x9) { sendFrame(0xA, p); continue; }
            if (op == 0xA) continue;
            if (op == 0x1 || op == 0x2) {
                msg = new ByteArrayOutputStream();
                binary = op == 0x2;
            } else if (op != 0x0 || msg == null) {
                throw new IOException("unexpected opcode " + op);
            }
            msg.write(p, 0, p.length);
            if (msg.size() > MAX_FRAME) throw new IOException("message too large");
            if (!fin) continue;
            if (binary) { msg = null; continue; }
            return new String(msg.toByteArray(), Crypto.UTF8);
        }
    }

    public void sendText(String s) throws IOException {
        sendFrame(0x1, s.getBytes(Crypto.UTF8));
    }

    private synchronized void sendFrame(int op, byte[] p) throws IOException {
        if (op == 0x8) {
            if (closeSent) return;
            closeSent = true;
        }
        ByteArrayOutputStream f = new ByteArrayOutputStream(p.length + 14);
        f.write(0x80 | op);
        if (p.length < 126) f.write(0x80 | p.length);
        else if (p.length < 65536) {
            f.write(0x80 | 126);
            f.write(p.length >>> 8);
            f.write(p.length & 0xff);
        } else {
            f.write(0x80 | 127);
            for (int i = 7; i >= 0; i--) f.write(i >= 4 ? 0 : (p.length >>> (8 * i)) & 0xff);
        }
        byte[] mask = new byte[4];
        maskRng.nextBytes(mask);
        f.write(mask, 0, 4);
        byte[] m = new byte[p.length];
        for (int i = 0; i < p.length; i++) m[i] = (byte) (p[i] ^ mask[i & 3]);
        f.write(m, 0, m.length);
        out.write(f.toByteArray());
        out.flush();
    }

    /** Drops the socket (never blocks on a stuck writer). Safe from any thread, any number of times. */
    public void close() {
        try { sock.close(); } catch (IOException ignored) { }
    }
}
