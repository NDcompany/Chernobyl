package com.chernobyl.kitkat;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertTrue;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.security.MessageDigest;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import org.json.JSONObject;
import org.junit.Test;

/**
 * Transport + WebSocket against an in-test companion double that speaks the real
 * wire contract (upgrade, hs_*, sealed frames as the host): endpoint failover,
 * the Origin header, hs_rekey persisted then acked, and close 4003 as terminal.
 */
public class TransportTest {
    static final byte[] PSK = CryptoTest.filled(32, 7);
    static final byte[] NEW_PSK = CryptoTest.filled(32, 9);

    /** Server side of one connection. */
    static final class Peer {
        final Socket s;
        final InputStream in;
        final OutputStream out;
        String request;

        Peer(Socket s) throws IOException {
            this.s = s;
            in = s.getInputStream();
            out = s.getOutputStream();
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            while (!b.toString("UTF-8").endsWith("\r\n\r\n")) b.write(in.read());
            request = b.toString("UTF-8");
            String key = null;
            for (String l : request.split("\r\n")) if (l.toLowerCase().startsWith("sec-websocket-key:")) key = l.substring(18).trim();
            String accept;
            try {
                accept = Crypto.b64encode(MessageDigest.getInstance("SHA-1").digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").getBytes("UTF-8")));
            } catch (Exception e) {
                throw new IOException(e);
            }
            out.write(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n").getBytes("UTF-8"));
        }

        String recv() throws IOException {
            int b0 = in.read(), b1 = in.read();
            assertTrue("client frames are masked", (b1 & 0x80) != 0);
            int len = b1 & 0x7f;
            if (len == 126) len = (in.read() << 8) | in.read();
            byte[] mask = new byte[4];
            readFully(mask);
            byte[] p = new byte[len];
            readFully(p);
            for (int i = 0; i < p.length; i++) p[i] ^= mask[i & 3];
            if ((b0 & 0x0f) != 1) return recv();
            return new String(p, "UTF-8");
        }

        void readFully(byte[] b) throws IOException {
            int o = 0;
            while (o < b.length) o += in.read(b, o, b.length - o);
        }

        void frame(int op, byte[] p) throws IOException {
            out.write(0x80 | op);
            if (p.length < 126) out.write(p.length);
            else { out.write(126); out.write(p.length >> 8); out.write(p.length & 0xff); }
            out.write(p);
            out.flush();
        }

        void send(String text) throws IOException {
            frame(1, text.getBytes("UTF-8"));
        }

        void close(int code) throws IOException {
            frame(8, new byte[] {(byte) (code >> 8), (byte) code});
        }

        /** Host side of the hs_* handshake with `psk`. */
        E2e.Session handshake(byte[] psk, String dev) throws Exception {
            JSONObject hello = new JSONObject(recv());
            assertEquals("hs_hello", hello.getString("type"));
            byte[] cnonce = Crypto.b64decode(hello.getString("cnonce"));
            byte[] nonce = Crypto.random(16), salt = Crypto.random(16);
            send(new JSONObject().put("type", "hs_challenge").put("nonce", Crypto.b64encode(nonce)).put("salt", Crypto.b64encode(salt)).toString());
            JSONObject proof = new JSONObject(recv());
            assertTrue("client proof", Arrays.equals(E2e.proof(psk, "client", dev, nonce, salt, cnonce), Crypto.b64decode(proof.getString("proof"))));
            send(new JSONObject().put("type", "hs_ok").put("serverProof", Crypto.b64encode(E2e.proof(psk, "server", dev, nonce, salt, cnonce))).toString());
            return E2e.Session.host(E2e.deriveKeys(psk, salt, cnonce));
        }
    }

    static final WebSocket.Opener PLAIN = new WebSocket.Opener() {
        @Override
        public Socket open(String host, int port, boolean tls, int timeoutMs) throws IOException {
            Socket s = new Socket();
            s.connect(new java.net.InetSocketAddress(host, port), timeoutMs);
            return s;
        }
    };

    @Test
    public void failoverHandshakeRekeyThenTerminalClose() throws Exception {
        final ServerSocket server = new ServerSocket(0, 5, InetAddress.getByName("127.0.0.1"));
        ServerSocket dead = new ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"));
        int deadPort = dead.getLocalPort();
        dead.close(); // nothing listens: the first endpoint is refused
        String pay = "{\"v\":1,\"host\":\"h\",\"ep\":[\"ws://127.0.0.1:" + deadPort + "\"],\"dev\":\"0123456789abcdef\",\"psk\":\""
                + Crypto.b64encode(PSK) + "\",\"chal\":\"" + Crypto.b64encode(new byte[16]) + "\",\"exp\":1900000000}";
        E2e.Pairing pairing = E2e.parsePairing(pay);
        final BlockingQueue<String> statuses = new LinkedBlockingQueue<String>();
        final BlockingQueue<JSONObject> messages = new LinkedBlockingQueue<JSONObject>();
        final String[] stored = new String[1];
        Transport t = new Transport(
                Arrays.asList("ws://127.0.0.1:" + deadPort + "/?token=t", "ws://127.0.0.1:" + server.getLocalPort() + "/?token=t"),
                pairing, new Transport.Listener() {
                    @Override public void onStatus(String s) { statuses.add(s); }
                    @Override public void onMessage(JSONObject m) { messages.add(m); }
                    @Override public boolean onRekey(String psk) { stored[0] = psk; return true; }
                }, PLAIN);
        t.retryBaseMs = 50;
        t.retryMaxMs = 100;
        t.connect();

        Peer p = new Peer(server.accept());
        assertTrue(p.request, p.request.startsWith("GET /?token=t HTTP/1.1"));
        assertTrue(p.request, p.request.contains("\r\nOrigin: chernobyl://app\r\n"));
        E2e.Session host = p.handshake(PSK, "0123456789abcdef");
        p.send(host.seal("{\"type\":\"hs_rekey\",\"psk\":\"" + Crypto.b64encode(NEW_PSK) + "\"}"));
        JSONObject hello = new JSONObject(host.open(p.recv()));
        assertEquals("hello", hello.getString("type"));
        assertEquals(2, hello.getInt("protocol"));
        assertEquals("hs_rekey_ack", new JSONObject(host.open(p.recv())).getString("type"));
        assertEquals(Crypto.b64encode(NEW_PSK), stored[0]);
        p.send(host.seal("{\"type\":\"hello_ack\",\"protocol\":2,\"serverTime\":1,\"lastSeq\":0,\"snapshot\":{\"hosts\":[],\"agents\":[],\"sessions\":[],\"permissions\":[],\"activity\":[]}}"));
        assertEquals("hello_ack", messages.poll(5, TimeUnit.SECONDS).getString("type"));

        // a command gets its ack through the sealed channel
        final BlockingQueue<String> acks = new LinkedBlockingQueue<String>();
        t.send(new JSONObject().put("type", "list_state").put("commandId", "c1"), new Transport.AckCallback() {
            @Override public void done(JSONObject ack, String error) { acks.add(ack != null ? ack.optString("commandId") : error); }
        });
        assertEquals("c1", new JSONObject(host.open(p.recv())).getString("commandId"));
        p.send(host.seal("{\"type\":\"ack\",\"commandId\":\"c1\",\"ok\":true}"));
        assertEquals("c1", acks.poll(5, TimeUnit.SECONDS));

        // the dropped connection reconnects, and the next handshake proves the rotated key
        p.s.close();
        Peer p2 = new Peer(server.accept());
        E2e.Session host2 = p2.handshake(NEW_PSK, "0123456789abcdef");
        assertEquals("hello", new JSONObject(host2.open(p2.recv())).getString("type"));
        p2.close(4003);
        long until = System.currentTimeMillis() + 5000;
        while (!Transport.DENIED.equals(t.status()) && System.currentTimeMillis() < until) Thread.sleep(20);
        assertEquals(Transport.DENIED, t.status());
        assertEquals("device revoked", t.deniedReason());
        List<String> seen = new java.util.ArrayList<String>(statuses);
        assertTrue(seen.toString(), seen.contains(Transport.RECONNECTING) && seen.contains(Transport.HANDSHAKING) && seen.contains(Transport.CONNECTED));
        t.dispose();
        server.close();
    }

    @Test
    public void backoffStaysWithinBounds() {
        for (int a = 0; a < 40; a++) {
            long lo = Transport.retryDelayMs(a, 1000, 15000, 0), hi = Transport.retryDelayMs(a, 1000, 15000, 0.999999);
            assertTrue(lo >= 500 && hi <= 15000 && lo <= hi);
        }
        assertEquals(500, Transport.retryDelayMs(0, 1000, 15000, 0));
        assertEquals(7500, Transport.retryDelayMs(10, 1000, 15000, 0));
    }
}
