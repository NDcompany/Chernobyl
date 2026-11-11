package com.chernobyl.kitkat;

import java.io.ByteArrayOutputStream;
import java.io.FileInputStream;
import java.io.InputStream;
import java.nio.charset.Charset;
import java.security.SecureRandom;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * The wire contract's primitives in plain Java (ADR-019): Android 4.4's JCE has
 * no ChaCha20-Poly1305, so ChaCha20, Poly1305 and the RFC 8439 AEAD are written
 * here and proven by CryptoTest against RFC 8439 and the repo's golden frames.
 * HMAC-SHA256 / SHA-256 come from javax.crypto, which API 19 has.
 * Base64 and UTF-8 are strict, byte-identical with packages/e2e (base64.ts, utf8.ts).
 */
public final class Crypto {
    private Crypto() {}

    public static final Charset UTF8 = Charset.forName("UTF-8");

    /* ---------------- ChaCha20 (RFC 8439 2.3) ---------------- */

    private static int le32(byte[] b, int o) {
        return (b[o] & 0xff) | (b[o + 1] & 0xff) << 8 | (b[o + 2] & 0xff) << 16 | (b[o + 3] & 0xff) << 24;
    }

    private static void qr(int[] x, int a, int b, int c, int d) {
        x[a] += x[b]; x[d] = Integer.rotateLeft(x[d] ^ x[a], 16);
        x[c] += x[d]; x[b] = Integer.rotateLeft(x[b] ^ x[c], 12);
        x[a] += x[b]; x[d] = Integer.rotateLeft(x[d] ^ x[a], 8);
        x[c] += x[d]; x[b] = Integer.rotateLeft(x[b] ^ x[c], 7);
    }

    /** One 64-byte keystream block. */
    public static byte[] chachaBlock(byte[] key, int counter, byte[] nonce) {
        int[] s = new int[16];
        s[0] = 0x61707865; s[1] = 0x3320646e; s[2] = 0x79622d32; s[3] = 0x6b206574;
        for (int i = 0; i < 8; i++) s[4 + i] = le32(key, i * 4);
        s[12] = counter;
        for (int i = 0; i < 3; i++) s[13 + i] = le32(nonce, i * 4);
        int[] x = s.clone();
        for (int i = 0; i < 10; i++) {
            qr(x, 0, 4, 8, 12); qr(x, 1, 5, 9, 13); qr(x, 2, 6, 10, 14); qr(x, 3, 7, 11, 15);
            qr(x, 0, 5, 10, 15); qr(x, 1, 6, 11, 12); qr(x, 2, 7, 8, 13); qr(x, 3, 4, 9, 14);
        }
        byte[] out = new byte[64];
        for (int i = 0; i < 16; i++) {
            int v = x[i] + s[i];
            out[i * 4] = (byte) v; out[i * 4 + 1] = (byte) (v >>> 8);
            out[i * 4 + 2] = (byte) (v >>> 16); out[i * 4 + 3] = (byte) (v >>> 24);
        }
        return out;
    }

    /** XOR `in` with the keystream starting at block `counter`. */
    public static byte[] chacha20(byte[] key, int counter, byte[] nonce, byte[] in) {
        if (key.length != 32 || nonce.length != 12) throw new IllegalArgumentException("chacha20 key/nonce size");
        byte[] out = new byte[in.length];
        for (int off = 0; off < in.length; off += 64, counter++) {
            byte[] ks = chachaBlock(key, counter, nonce);
            int n = Math.min(64, in.length - off);
            for (int i = 0; i < n; i++) out[off + i] = (byte) (in[off + i] ^ ks[i]);
        }
        return out;
    }

    /* ---------------- Poly1305 (RFC 8439 2.5, 26-bit limbs) ---------------- */

    private static long u32(byte[] b, int o) {
        return le32(b, o) & 0xffffffffL;
    }

    public static byte[] poly1305(byte[] key, byte[] msg) {
        final long M = 0x3ffffff;
        long r0 = u32(key, 0) & 0x3ffffff, r1 = (u32(key, 3) >>> 2) & 0x3ffff03, r2 = (u32(key, 6) >>> 4) & 0x3ffc0ff,
                r3 = (u32(key, 9) >>> 6) & 0x3f03fff, r4 = (u32(key, 12) >>> 8) & 0x00fffff;
        long s1 = r1 * 5, s2 = r2 * 5, s3 = r3 * 5, s4 = r4 * 5;
        long h0 = 0, h1 = 0, h2 = 0, h3 = 0, h4 = 0;
        byte[] blk = new byte[17];
        for (int off = 0; off < msg.length; off += 16) {
            int n = Math.min(16, msg.length - off);
            java.util.Arrays.fill(blk, (byte) 0);
            System.arraycopy(msg, off, blk, 0, n);
            blk[n] = 1; // the high bit above the block (2^128 for full blocks)
            long hibit = n == 16 ? (1L << 24) : 0;
            h0 += u32(blk, 0) & M;
            h1 += (u32(blk, 3) >>> 2) & M;
            h2 += (u32(blk, 6) >>> 4) & M;
            h3 += (u32(blk, 9) >>> 6) & M;
            h4 += (u32(blk, 12) >>> 8) | hibit;
            long d0 = h0 * r0 + h1 * s4 + h2 * s3 + h3 * s2 + h4 * s1;
            long d1 = h0 * r1 + h1 * r0 + h2 * s4 + h3 * s3 + h4 * s2;
            long d2 = h0 * r2 + h1 * r1 + h2 * r0 + h3 * s4 + h4 * s3;
            long d3 = h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + h4 * s4;
            long d4 = h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0;
            long c = d0 >>> 26; h0 = d0 & M;
            d1 += c; c = d1 >>> 26; h1 = d1 & M;
            d2 += c; c = d2 >>> 26; h2 = d2 & M;
            d3 += c; c = d3 >>> 26; h3 = d3 & M;
            d4 += c; c = d4 >>> 26; h4 = d4 & M;
            h0 += c * 5; c = h0 >>> 26; h0 &= M; h1 += c;
        }
        long c = h1 >>> 26; h1 &= M;
        h2 += c; c = h2 >>> 26; h2 &= M;
        h3 += c; c = h3 >>> 26; h3 &= M;
        h4 += c; c = h4 >>> 26; h4 &= M;
        h0 += c * 5; c = h0 >>> 26; h0 &= M;
        h1 += c;
        long g0 = h0 + 5; c = g0 >>> 26; g0 &= M;
        long g1 = h1 + c; c = g1 >>> 26; g1 &= M;
        long g2 = h2 + c; c = g2 >>> 26; g2 &= M;
        long g3 = h3 + c; c = g3 >>> 26; g3 &= M;
        long g4 = h4 + c - (1L << 26);
        long mask = (g4 >>> 63) - 1; // g4 < 0 -> 0 (keep h), else all ones (take g)
        h0 = (h0 & ~mask) | (g0 & mask);
        h1 = (h1 & ~mask) | (g1 & mask);
        h2 = (h2 & ~mask) | (g2 & mask);
        h3 = (h3 & ~mask) | (g3 & mask);
        h4 = (h4 & ~mask) | (g4 & mask);
        long w0 = (h0 | (h1 << 26)) & 0xffffffffL;
        long w1 = ((h1 >>> 6) | (h2 << 20)) & 0xffffffffL;
        long w2 = ((h2 >>> 12) | (h3 << 14)) & 0xffffffffL;
        long w3 = ((h3 >>> 18) | (h4 << 8)) & 0xffffffffL;
        long f = w0 + u32(key, 16); w0 = f & 0xffffffffL;
        f = w1 + u32(key, 20) + (f >>> 32); w1 = f & 0xffffffffL;
        f = w2 + u32(key, 24) + (f >>> 32); w2 = f & 0xffffffffL;
        f = w3 + u32(key, 28) + (f >>> 32); w3 = f & 0xffffffffL;
        byte[] tag = new byte[16];
        long[] w = {w0, w1, w2, w3};
        for (int i = 0; i < 4; i++)
            for (int j = 0; j < 4; j++) tag[i * 4 + j] = (byte) (w[i] >>> (8 * j));
        return tag;
    }

    /* ---------------- AEAD (RFC 8439 2.8) ---------------- */

    private static byte[] macData(byte[] aad, byte[] ct) {
        ByteArrayOutputStream o = new ByteArrayOutputStream(aad.length + ct.length + 48);
        o.write(aad, 0, aad.length);
        o.write(new byte[(16 - aad.length % 16) % 16], 0, (16 - aad.length % 16) % 16);
        o.write(ct, 0, ct.length);
        o.write(new byte[(16 - ct.length % 16) % 16], 0, (16 - ct.length % 16) % 16);
        long[] lens = {aad.length, ct.length};
        for (long l : lens) for (int i = 0; i < 8; i++) o.write((int) (l >>> (8 * i)) & 0xff);
        return o.toByteArray();
    }

    private static byte[] polyKey(byte[] key, byte[] nonce) {
        byte[] k = new byte[32];
        System.arraycopy(chachaBlock(key, 0, nonce), 0, k, 0, 32);
        return k;
    }

    /** ciphertext ‖ 16-byte tag. */
    public static byte[] seal(byte[] key, byte[] nonce, byte[] aad, byte[] plain) {
        byte[] ct = chacha20(key, 1, nonce, plain);
        byte[] tag = poly1305(polyKey(key, nonce), macData(aad, ct));
        byte[] out = new byte[ct.length + 16];
        System.arraycopy(ct, 0, out, 0, ct.length);
        System.arraycopy(tag, 0, out, ct.length, 16);
        return out;
    }

    /** Plaintext, or null when the tag does not verify. */
    public static byte[] open(byte[] key, byte[] nonce, byte[] aad, byte[] sealed) {
        if (sealed.length < 16) return null;
        byte[] ct = new byte[sealed.length - 16];
        System.arraycopy(sealed, 0, ct, 0, ct.length);
        byte[] tag = new byte[16];
        System.arraycopy(sealed, ct.length, tag, 0, 16);
        if (!constantTimeEq(poly1305(polyKey(key, nonce), macData(aad, ct)), tag)) return null;
        return chacha20(key, 1, nonce, ct);
    }

    /* ---------------- HMAC / HKDF / SHA-256 ---------------- */

    public static byte[] hmac(byte[] key, byte[] data) {
        try {
            Mac m = Mac.getInstance("HmacSHA256");
            // SecretKeySpec refuses an empty key; HMAC zero-pads keys, so one zero byte is the same key
            m.init(new SecretKeySpec(key.length == 0 ? new byte[1] : key, "HmacSHA256"));
            return m.doFinal(data);
        } catch (Exception e) {
            throw new IllegalStateException("HmacSHA256 unavailable", e);
        }
    }

    public static byte[] sha256(byte[] data) {
        try {
            return java.security.MessageDigest.getInstance("SHA-256").digest(data);
        } catch (Exception e) {
            throw new IllegalStateException("SHA-256 unavailable", e);
        }
    }

    /** RFC 5869 extract + expand. */
    public static byte[] hkdf(byte[] ikm, byte[] salt, byte[] info, int len) {
        byte[] prk = hmac(salt, ikm);
        byte[] out = new byte[len];
        byte[] t = new byte[0];
        for (int i = 1, o = 0; o < len; i++) {
            byte[] in = concat(t, info, new byte[] {(byte) i});
            t = hmac(prk, in);
            int n = Math.min(t.length, len - o);
            System.arraycopy(t, 0, out, o, n);
            o += n;
        }
        return out;
    }

    public static byte[] concat(byte[]... parts) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        for (byte[] p : parts) o.write(p, 0, p.length);
        return o.toByteArray();
    }

    /** Never short-circuits on the first difference. */
    public static boolean constantTimeEq(byte[] a, byte[] b) {
        if (a.length != b.length) return false;
        int d = 0;
        for (int i = 0; i < a.length; i++) d |= a[i] ^ b[i];
        return d == 0;
    }

    /* ---------------- CSPRNG ---------------- */

    private static SecureRandom rng;

    /**
     * Android 4.1-4.3 shipped a badly seeded SHA1PRNG (2013 advisory); 4.4 fixed
     * it, but a device can run an unpatched build. Bytes from /dev/urandom are
     * read directly and mixed (XOR) with SecureRandom, so either alone suffices.
     */
    public static synchronized byte[] random(int n) {
        if (rng == null) rng = new SecureRandom();
        byte[] a = new byte[n];
        rng.nextBytes(a);
        InputStream in = null;
        try {
            in = new FileInputStream("/dev/urandom");
            byte[] u = new byte[n];
            int got = 0;
            while (got < n) {
                int r = in.read(u, got, n - got);
                if (r < 0) break;
                got += r;
            }
            for (int i = 0; i < got; i++) a[i] ^= u[i];
        } catch (Exception ignored) {
            // no /dev/urandom (a JVM on Windows): SecureRandom alone
        } finally {
            try { if (in != null) in.close(); } catch (Exception ignored) { }
        }
        return a;
    }

    /* ---------------- Base64 (strict; matches base64.ts) ---------------- */

    private static final char[] ALPHA = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".toCharArray();

    public static String b64encode(byte[] b) {
        StringBuilder s = new StringBuilder((b.length + 2) / 3 * 4);
        for (int i = 0; i < b.length; i += 3) {
            int b0 = b[i] & 0xff, b1 = i + 1 < b.length ? b[i + 1] & 0xff : 0, b2 = i + 2 < b.length ? b[i + 2] & 0xff : 0;
            s.append(ALPHA[b0 >> 2]).append(ALPHA[((b0 & 3) << 4) | (b1 >> 4)]);
            s.append(i + 1 < b.length ? ALPHA[((b1 & 15) << 2) | (b2 >> 6)] : '=');
            s.append(i + 2 < b.length ? ALPHA[b2 & 63] : '=');
        }
        return s.toString();
    }

    private static int b64val(char c) {
        if (c >= 'A' && c <= 'Z') return c - 'A';
        if (c >= 'a' && c <= 'z') return c - 'a' + 26;
        if (c >= '0' && c <= '9') return c - '0' + 52;
        if (c == '+') return 62;
        if (c == '/') return 63;
        return -1;
    }

    /** Null on any malformed input (bad alphabet, impossible length, stray padding, non-zero trailing bits). */
    public static byte[] b64decode(String text) {
        int end = text.length();
        while (end > 0 && text.charAt(end - 1) == '=') end--;
        if (end % 4 == 1) return null;
        byte[] out = new byte[end * 3 / 4];
        int acc = 0, bits = 0, o = 0;
        for (int i = 0; i < end; i++) {
            int v = b64val(text.charAt(i));
            if (v < 0) return null;
            acc = (acc << 6) | v;
            bits += 6;
            if (bits >= 8) {
                bits -= 8;
                out[o++] = (byte) (acc >> bits);
            }
            acc &= 0xff; // only the low bits matter
        }
        if (bits > 0 && (acc & ((1 << bits) - 1)) != 0) return null;
        if (o == out.length) return out;
        byte[] r = new byte[o];
        System.arraycopy(out, 0, r, 0, o);
        return r;
    }

    /* ---------------- UTF-8 (strict; Rust String::from_utf8 semantics) ---------------- */

    /** Null for overlongs, surrogates, > U+10FFFF, truncated or stray bytes. */
    public static String utf8Decode(byte[] b) {
        int i = 0;
        while (i < b.length) {
            int c = b[i] & 0xff;
            int need, lo = 0x80, hi = 0xbf;
            if (c < 0x80) { i++; continue; }
            else if (c >= 0xc2 && c <= 0xdf) need = 1;
            else if (c == 0xe0) { need = 2; lo = 0xa0; }
            else if (c >= 0xe1 && c <= 0xec || c == 0xee || c == 0xef) need = 2;
            else if (c == 0xed) { need = 2; hi = 0x9f; }
            else if (c == 0xf0) { need = 3; lo = 0x90; }
            else if (c >= 0xf1 && c <= 0xf3) need = 3;
            else if (c == 0xf4) { need = 3; hi = 0x8f; }
            else return null;
            if (i + need >= b.length) return null; // truncated sequence
            int c1 = b[i + 1] & 0xff;
            if (c1 < lo || c1 > hi) return null;
            for (int k = 2; k <= need; k++) {
                int ck = b[i + k] & 0xff;
                if (ck < 0x80 || ck > 0xbf) return null;
            }
            i += need + 1;
        }
        return new String(b, UTF8);
    }

    public static byte[] utf8(String s) {
        return s.getBytes(UTF8);
    }
}
