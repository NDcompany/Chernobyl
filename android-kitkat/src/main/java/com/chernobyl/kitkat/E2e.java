package com.chernobyl.kitkat;

import java.util.ArrayList;
import java.util.List;
import java.util.regex.Pattern;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * Pairing payload, handshake proof, key derivation and sealed frames: a line-by-line
 * port of packages/e2e (pairing.ts, handshake.ts, session.ts), byte-identical with
 * services/companion-rs/src/e2e.rs.
 */
public final class E2e {
    private E2e() {}

    /** Stable failure `code` like the TS E2eError / PairingError. */
    public static final class E2eException extends Exception {
        public final String code;

        public E2eException(String code, String message) {
            super(message);
            this.code = code;
        }
    }

    /* ---------------- pairing payload (pairing.ts) ---------------- */

    public static final class Pairing {
        public final String host;
        public final List<String> endpoints;
        public final String deviceId;
        public final byte[] psk;
        public final byte[] challenge;
        public final long expiresAtSec;

        Pairing(String host, List<String> endpoints, String deviceId, byte[] psk, byte[] challenge, long exp) {
            this.host = host;
            this.endpoints = endpoints;
            this.deviceId = deviceId;
            this.psk = psk;
            this.challenge = challenge;
            this.expiresAtSec = exp;
        }

        public Pairing withPsk(byte[] newPsk) {
            return new Pairing(host, endpoints, deviceId, newPsk, challenge, expiresAtSec);
        }
    }

    private static final Pattern EP = Pattern.compile("^wss?://\\S+$");
    private static final Pattern DEV = Pattern.compile("^[0-9a-f]{16}$");

    static boolean isInt(Object o) {
        return o instanceof Integer || o instanceof Long;
    }

    /** The terminal prints `payload: {...}`: either form is accepted. */
    public static Pairing parsePairing(String raw) throws E2eException {
        String text = raw.trim();
        if (text.startsWith("payload:")) text = text.substring(8).trim();
        JSONObject o;
        try {
            o = new JSONObject(text);
        } catch (Exception e) {
            throw new E2eException("malformed", "pairing payload is not valid JSON");
        }
        if (!(isInt(o.opt("v")) && o.optLong("v") == 1))
            throw new E2eException("unsupported-version", "unsupported pairing payload version: " + o.opt("v"));
        Object host = o.opt("host");
        if (!(host instanceof String) || ((String) host).isEmpty())
            throw new E2eException("malformed", "payload must carry a host id (host)");
        JSONArray ep = o.optJSONArray("ep");
        if (ep == null || ep.length() == 0)
            throw new E2eException("bad-endpoint", "payload must carry at least one endpoint (ep)");
        List<String> eps = new ArrayList<String>();
        for (int i = 0; i < ep.length(); i++) {
            Object e = ep.opt(i);
            if (!(e instanceof String) || !EP.matcher((String) e).matches())
                throw new E2eException("bad-endpoint", "endpoint is not a ws(s) URL: " + e);
            eps.add((String) e);
        }
        Object dev = o.opt("dev");
        if (!(dev instanceof String))
            throw new E2eException("missing-device-id", "payload must carry the device id (dev) - see ADR-012");
        if (!DEV.matcher((String) dev).matches())
            throw new E2eException("malformed", "device id (dev) must be 16 lowercase hex characters");
        byte[] psk = o.opt("psk") instanceof String ? Crypto.b64decode(o.optString("psk")) : null;
        if (psk == null || psk.length != 32) throw new E2eException("bad-psk", "psk must be base64 of exactly 32 bytes");
        byte[] chal = o.opt("chal") instanceof String ? Crypto.b64decode(o.optString("chal")) : null;
        if (chal == null || chal.length != 16) throw new E2eException("bad-psk", "chal must be base64 of exactly 16 bytes");
        Object exp = o.opt("exp");
        if (!isInt(exp) || ((Number) exp).longValue() <= 0)
            throw new E2eException("bad-expiry", "exp must be a positive unix-seconds integer");
        return new Pairing((String) host, eps, (String) dev, psk, chal, ((Number) exp).longValue());
    }

    /* ---------------- handshake (handshake.ts) ---------------- */

    /** HMAC-SHA256(psk, role ‖ 0 ‖ deviceId ‖ 0 ‖ nonce ‖ 0 ‖ salt ‖ 0 ‖ cnonce). */
    public static byte[] proof(byte[] psk, String role, String deviceId, byte[] nonce, byte[] salt, byte[] cnonce) {
        byte[] z = {0};
        return Crypto.hmac(psk, Crypto.concat(Crypto.utf8(role), z, Crypto.utf8(deviceId), z, nonce, z, salt, z, cnonce));
    }

    /** {c2h, h2c}: HKDF salt = server salt ‖ client nonce. */
    public static byte[][] deriveKeys(byte[] psk, byte[] salt, byte[] cnonce) {
        byte[] s = Crypto.concat(salt, cnonce);
        return new byte[][] {
            Crypto.hkdf(psk, s, Crypto.utf8("chernobyl-e2e-c2h"), 32),
            Crypto.hkdf(psk, s, Crypto.utf8("chernobyl-e2e-h2c"), 32),
        };
    }

    /** Bytes 0-7 big-endian counter, 8-11 zero (e2e.rs counter_nonce). */
    public static byte[] counterNonce(long counter) {
        byte[] n = new byte[12];
        for (int i = 7; i >= 0; i--) {
            n[i] = (byte) counter;
            counter >>>= 8;
        }
        return n;
    }

    static JSONObject readJson(String text) throws E2eException {
        try {
            return new JSONObject(text);
        } catch (Exception e) {
            throw new E2eException("malformed", "handshake frame is not a JSON object");
        }
    }

    static byte[] fixedB64(JSONObject f, String field, int len) throws E2eException {
        byte[] b = f.opt(field) instanceof String ? Crypto.b64decode(f.optString(field)) : null;
        if (b == null || b.length != len) throw new E2eException("malformed", field + " must be base64 of " + len + " bytes");
        return b;
    }

    static void throwIfErr(JSONObject f) throws E2eException {
        if ("hs_err".equals(f.opt("type")))
            throw new E2eException("handshake-rejected", f.opt("reason") instanceof String ? f.optString("reason") : "no reason given");
    }

    /** Blocking frame I/O the handshake runs over (one connection). */
    public interface Io {
        void send(String text) throws java.io.IOException;

        String recv() throws java.io.IOException;
    }

    /** One full client handshake (performHandshake). */
    public static Session handshake(Io io, Pairing p) throws E2eException, java.io.IOException {
        byte[] cnonce = Crypto.random(16);
        io.send("{\"type\":\"hs_hello\",\"deviceId\":" + JSONObject.quote(p.deviceId) + ",\"cnonce\":\"" + Crypto.b64encode(cnonce) + "\"}");
        JSONObject ch = readJson(io.recv());
        throwIfErr(ch);
        if (!"hs_challenge".equals(ch.opt("type"))) throw new E2eException("malformed", "expected hs_challenge, got " + ch.opt("type"));
        byte[] nonce = fixedB64(ch, "nonce", 16);
        byte[] salt = fixedB64(ch, "salt", 16);
        io.send("{\"type\":\"hs_proof\",\"proof\":\"" + Crypto.b64encode(proof(p.psk, "client", p.deviceId, nonce, salt, cnonce)) + "\"}");
        JSONObject ok = readJson(io.recv());
        throwIfErr(ok);
        if (!"hs_ok".equals(ok.opt("type"))) throw new E2eException("malformed", "expected hs_ok, got " + ok.opt("type"));
        byte[] sp = fixedB64(ok, "serverProof", 32);
        if (!Crypto.constantTimeEq(proof(p.psk, "server", p.deviceId, nonce, salt, cnonce), sp))
            throw new E2eException("server-proof-mismatch", "server failed mutual authentication");
        return Session.client(deriveKeys(p.psk, salt, cnonce));
    }

    /* ---------------- sealed frames (session.ts) ---------------- */

    public static final class Session {
        private final byte[] sendKey, recvKey;
        private long tx = 0, rx = 0;

        Session(byte[] sendKey, byte[] recvKey) {
            this.sendKey = sendKey;
            this.recvKey = recvKey;
        }

        public static Session client(byte[][] k) {
            return new Session(k[0], k[1]);
        }

        public static Session host(byte[][] k) {
            return new Session(k[1], k[0]);
        }

        public synchronized String seal(String plaintext) {
            byte[] sealed = Crypto.seal(sendKey, counterNonce(tx), new byte[0], Crypto.utf8(plaintext));
            String env = "{\"n\":" + tx + ",\"d\":\"" + Crypto.b64encode(sealed) + "\"}";
            tx++;
            return env;
        }

        /** Throws on replay/desync/tamper/garbage without advancing rx. */
        public synchronized String open(String envelope) throws E2eException {
            JSONObject o;
            try {
                o = new JSONObject(envelope);
            } catch (Exception e) {
                throw new E2eException("malformed", "malformed sealed envelope");
            }
            Object n = o.opt("n");
            if (!isInt(n) || ((Number) n).longValue() < 0 || ((Number) n).longValue() > 9007199254740991L)
                throw new E2eException("malformed", "missing or invalid counter");
            if (!(o.opt("d") instanceof String)) throw new E2eException("malformed", "missing body");
            long c = ((Number) n).longValue();
            if (c < rx) throw new E2eException("replay", "frame replay (counter " + c + " < expected " + rx + ")");
            if (c > rx) throw new E2eException("desync", "counter desync (counter " + c + " > expected " + rx + ")");
            byte[] sealed = Crypto.b64decode(o.optString("d"));
            if (sealed == null) throw new E2eException("malformed", "bad base64 body");
            byte[] plain = Crypto.open(recvKey, counterNonce(c), new byte[0], sealed);
            if (plain == null) throw new E2eException("aead", "AEAD authentication failed");
            String text = Crypto.utf8Decode(plain);
            if (text == null) throw new E2eException("bad-utf8", "sealed body not valid UTF-8");
            rx++;
            return text;
        }
    }
}
