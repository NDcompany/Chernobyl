package com.chernobyl.kitkat;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotEquals;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import java.io.InputStream;
import java.util.Arrays;
import java.util.Scanner;
import org.json.JSONArray;
import org.json.JSONObject;
import org.junit.Test;

/** RFC 8439 / FIPS 180-4 / RFC 4231 / RFC 5869 vectors and packages/e2e kats.test.ts + session.test.ts, one-for-one. */
public class CryptoTest {
    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x & 0xff));
        return s.toString();
    }

    static byte[] unhex(String h) {
        h = h.replaceAll("[\\s:]", "");
        byte[] b = new byte[h.length() / 2];
        for (int i = 0; i < b.length; i++) b[i] = (byte) Integer.parseInt(h.substring(i * 2, i * 2 + 2), 16);
        return b;
    }

    static byte[] seq(int from, int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) b[i] = (byte) (from + i);
        return b;
    }

    static byte[] filled(int n, int v) {
        byte[] b = new byte[n];
        Arrays.fill(b, (byte) v);
        return b;
    }

    static final String SUNSCREEN =
            "Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";

    @Test
    public void chacha20BlockRfc8439_2_3_2() {
        byte[] key = seq(0, 32);
        byte[] nonce = unhex("000000090000004a00000000");
        assertEquals("10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e"
                + "d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e",
                hex(Crypto.chachaBlock(key, 1, nonce)));
    }

    @Test
    public void chacha20EncryptRfc8439_2_4_2() {
        byte[] ct = Crypto.chacha20(seq(0, 32), 1, unhex("000000000000004a00000000"), Crypto.utf8(SUNSCREEN));
        assertEquals("6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b"
                + "f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8"
                + "07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736"
                + "5af90bbf74a35be6b40b8eedf2785e42874d", hex(ct));
    }

    @Test
    public void poly1305Rfc8439_2_5_2() {
        byte[] key = unhex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
        assertEquals("a8061dc1305136c6c22b8baf0c0127a9", hex(Crypto.poly1305(key, Crypto.utf8("Cryptographic Forum Research Group"))));
    }

    @Test
    public void poly1305EdgeCasesRfc8439AppendixA3() {
        // #5: h reaches p - 2^130-5 reduction
        assertEquals("03000000000000000000000000000000", hex(Crypto.poly1305(
                unhex("02000000000000000000000000000000" + "00000000000000000000000000000000"), filled(16, 0xff))));
        // #6: final addition carries
        assertEquals("03000000000000000000000000000000", hex(Crypto.poly1305(
                unhex("02000000000000000000000000000000" + "ffffffffffffffffffffffffffffffff"),
                unhex("02000000000000000000000000000000"))));
        // wraps to exactly zero (checked against pyca/cryptography)
        assertEquals("00000000000000000000000000000000", hex(Crypto.poly1305(
                unhex("01000000000000000000000000000000" + "00000000000000000000000000000000"),
                unhex("ffffffffffffffffffffffffffffffff" + "fbfefefefefefefefefefefefefefefe" + "01010101010101010101010101010101"))));
    }

    @Test
    public void poly1305MatchesPycaAcrossLengthsAndAllOnes() throws Exception {
        InputStream in = getClass().getClassLoader().getResourceAsStream("poly1305-py.json");
        JSONArray cases = new JSONArray(new Scanner(in, "UTF-8").useDelimiter("\\A").next());
        for (int i = 0; i < cases.length(); i++) {
            JSONObject c = cases.getJSONObject(i);
            assertEquals(c.getString("msg"), c.getString("tag"), hex(Crypto.poly1305(unhex(c.getString("key")), unhex(c.getString("msg")))));
        }
    }

    @Test
    public void aeadRfc8439_2_8_2() {
        byte[] key = seq(0x80, 32);
        byte[] nonce = unhex("070000004041424344454647");
        byte[] aad = unhex("50515253c0c1c2c3c4c5c6c7");
        byte[] sealed = Crypto.seal(key, nonce, aad, Crypto.utf8(SUNSCREEN));
        assertEquals("1ae10b594f09e26a7e902ecbd0600691", hex(Arrays.copyOfRange(sealed, sealed.length - 16, sealed.length)));
        assertEquals("d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6",
                hex(Arrays.copyOfRange(sealed, 0, 32)));
        assertEquals(SUNSCREEN, new String(Crypto.open(key, nonce, aad, sealed), Crypto.UTF8));
        sealed[3] ^= 1;
        assertNull("tampered ciphertext must fail", Crypto.open(key, nonce, aad, sealed));
    }

    @Test
    public void aeadMatchesNodeOpenSslAcrossLengths() throws Exception {
        InputStream in = getClass().getClassLoader().getResourceAsStream("aead-node.json");
        assertNotNull(in);
        JSONArray cases = new JSONArray(new Scanner(in, "UTF-8").useDelimiter("\\A").next());
        assertTrue(cases.length() > 10);
        for (int i = 0; i < cases.length(); i++) {
            JSONObject c = cases.getJSONObject(i);
            byte[] key = unhex(c.getString("key")), nonce = unhex(c.getString("nonce")), aad = unhex(c.getString("aad"));
            byte[] pt = unhex(c.getString("pt"));
            assertEquals("len " + pt.length, c.getString("sealed"), hex(Crypto.seal(key, nonce, aad, pt)));
            assertArrayEquals(pt, Crypto.open(key, nonce, aad, unhex(c.getString("sealed"))));
        }
    }

    @Test
    public void sha256Fips180() {
        assertEquals("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", hex(Crypto.sha256(new byte[0])));
        assertEquals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", hex(Crypto.sha256(Crypto.utf8("abc"))));
    }

    @Test
    public void hmacRfc4231() {
        assertEquals("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
                hex(Crypto.hmac(filled(20, 0x0b), Crypto.utf8("Hi There"))));
        assertEquals("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
                hex(Crypto.hmac(Crypto.utf8("Jefe"), Crypto.utf8("what do ya want for nothing?"))));
        assertEquals("773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
                hex(Crypto.hmac(filled(20, 0xaa), filled(50, 0xdd))));
        assertEquals("82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
                hex(Crypto.hmac(seq(1, 25), filled(50, 0xcd))));
    }

    @Test
    public void hkdfRfc5869Case1() {
        assertEquals("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
                hex(Crypto.hkdf(filled(22, 0x0b), seq(0, 13), seq(0xf0, 10), 42)));
    }

    @Test
    public void base64MatchesBase64Ts() {
        assertEquals("AAA=", Crypto.b64encode(new byte[] {0, 0}));
        assertEquals("+/8=", Crypto.b64encode(new byte[] {(byte) 0xfb, (byte) 0xff}));
        assertEquals("AQIDBAU=", Crypto.b64encode(new byte[] {1, 2, 3, 4, 5}));
        byte[] all = seq(0, 256);
        assertArrayEquals(all, Crypto.b64decode(Crypto.b64encode(all)));
        assertNull("bad alphabet", Crypto.b64decode("!!"));
        assertNull("impossible length", Crypto.b64decode("AAAAA"));
        assertNull("padding in the middle", Crypto.b64decode("AA=A"));
        assertNull("non-zero trailing bits", Crypto.b64decode("AAB="));
        assertNotNull("unpadded input is accepted", Crypto.b64decode("AQIDBAU"));
    }

    @Test
    public void utf8MatchesRustFromUtf8() {
        String s = "cůžek 日本語 🚀 tail";
        assertEquals(s, Crypto.utf8Decode(Crypto.utf8(s)));
        int[][] bad = {{0x80, 0x81}, {0xc0, 0x80}, {0xe0, 0x80, 0xaf}, {0xe2, 0x82}, {0xf0, 0x9f, 0x9a},
            {0xed, 0xa0, 0x80}, {0xf4, 0x90, 0x80, 0x80}, {0xff}};
        for (int[] b : bad) {
            byte[] x = new byte[b.length];
            for (int i = 0; i < b.length; i++) x[i] = (byte) b[i];
            assertNull(hex(x), Crypto.utf8Decode(x));
        }
    }

    /* ---------------- handshake + sealed frames (kats.test.ts, session.test.ts) ---------------- */

    static final byte[] PSK = filled(32, 7);
    static final byte[] SALT = Crypto.utf8("SALTSALTSALTSALT");
    static final byte[] CNONCE = Crypto.utf8("CNONCNONCNONCNON");

    @Test
    public void goldenFramesByteExactWithE2eRs() throws Exception {
        byte[][] keys = E2e.deriveKeys(PSK, SALT, CNONCE);
        E2e.Session client = E2e.Session.client(keys), host = E2e.Session.host(keys);
        assertEquals("{\"n\":0,\"d\":\"r6Rs7v9M2/y/CAkkvJJ+xqHQUhv8\"}", client.seal("hello"));
        assertEquals("{\"n\":1,\"d\":\"HSVsxvDYQ7qYlBmBoyBF2XmxHNw=\"}", client.seal("ping"));
        assertEquals("{\"n\":0,\"d\":\"zLVPpIF5s4rcEesgzm6PwVXf3OuKDblSmQ==\"}", host.seal("hello_ack"));
        assertEquals("{\"n\":1,\"d\":\"e6r062GVTFNYUGpBRQ0Ue4hoGLI=\"}", host.seal("pong"));
        E2e.Session c2 = E2e.Session.client(keys);
        assertEquals("hello_ack", c2.open("{\"n\":0,\"d\":\"zLVPpIF5s4rcEesgzm6PwVXf3OuKDblSmQ==\"}"));
        assertEquals("pong", c2.open("{\"n\":1,\"d\":\"e6r062GVTFNYUGpBRQ0Ue4hoGLI=\"}"));
    }

    @Test
    public void proofBindsEverything() {
        byte[] nonce = Crypto.utf8("NONCENONCENONCE"), salt = Crypto.utf8("SALTSALTSALTSALT"), cn = Crypto.utf8("CNONCNONCNONCNON");
        String a = hex(E2e.proof(PSK, "client", "dev1", nonce, salt, cn));
        assertNotEquals(a, hex(E2e.proof(PSK, "server", "dev1", nonce, salt, cn)));
        assertNotEquals(a, hex(E2e.proof(PSK, "client", "dev2", nonce, salt, cn)));
        assertNotEquals(a, hex(E2e.proof(PSK, "client", "dev1", Crypto.utf8("OTHERNONCEOTHER"), salt, cn)));
        assertNotEquals(a, hex(E2e.proof(PSK, "client", "dev1", nonce, Crypto.utf8("OTHERSALTOTHERS"), cn)));
        assertNotEquals(a, hex(E2e.proof(PSK, "client", "dev1", nonce, salt, Crypto.utf8("OTHERCNONCEOTHER"))));
        assertNotEquals(a, hex(E2e.proof(filled(32, 8), "client", "dev1", nonce, salt, cn)));
    }

    @Test
    public void keysDeterministicDirectionalAndCnonceBound() {
        byte[][] a = E2e.deriveKeys(PSK, filled(16, 9), CNONCE), b = E2e.deriveKeys(PSK, filled(16, 9), CNONCE);
        assertArrayEquals(a[0], b[0]);
        assertArrayEquals(a[1], b[1]);
        assertFalse(Arrays.equals(a[0], a[1]));
        assertFalse(Arrays.equals(a[0], E2e.deriveKeys(PSK, filled(16, 9), filled(16, 1))[0]));
        assertFalse(Arrays.equals(a[1], E2e.deriveKeys(PSK, filled(16, 9), filled(16, 1))[1]));
    }

    @Test
    public void counterNonceIsBigEndianThenZeros() {
        assertEquals("000000000000000100000000", hex(E2e.counterNonce(1)));
        assertEquals("000000000000010200000000", hex(E2e.counterNonce(258)));
    }

    static void expectCode(String code, E2e.Session s, String env) {
        try {
            s.open(env);
            fail("must reject: " + env);
        } catch (E2e.E2eException e) {
            assertEquals(env, code, e.code);
        }
    }

    @Test
    public void replayDesyncTamperAndHostileEnvelopes() throws Exception {
        byte[][] keys = E2e.deriveKeys(PSK, SALT, CNONCE);
        E2e.Session client = E2e.Session.client(keys), host = E2e.Session.host(keys);
        String f0 = client.seal("legit-0");
        String[] hostile = {"not json", "", "{\"n\":0}", "{\"d\":\"AAAA\"}", "{\"n\":-1,\"d\":\"AAAA\"}", "{\"n\":1.5,\"d\":\"AAAA\"}",
            "{\"n\":99999999999999999999,\"d\":\"AAAA\"}", "{\"n\":0,\"d\":\"!!\"}", "{\"n\":0,\"d\":\"AAAA\"}"};
        for (String h : hostile) {
            try {
                host.open(h);
                fail("must reject " + h);
            } catch (E2e.E2eException e) {
                assertFalse(e.code.isEmpty());
            }
        }
        assertEquals("legit-0", host.open(f0));
        expectCode("replay", host, f0);
        String f1 = client.seal("legit-1"), f2 = client.seal("legit-2");
        expectCode("desync", host, f2);
        assertEquals("legit-1", host.open(f1));
        assertEquals("legit-2", host.open(f2));
        String f3 = client.seal("secret message");
        int at = f3.lastIndexOf('"') - 1;
        String flipped = f3.substring(0, at) + (f3.charAt(at) == 'A' ? 'B' : 'A') + f3.substring(at + 1);
        expectCode("aead", host, flipped);
        String u = "žluťoučký 🚀 kůň";
        assertEquals(u, host.open(f3).equals("secret message") ? host.open(client.seal(u)) : null);
        E2e.Session evil = E2e.Session.client(E2e.deriveKeys(PSK, filled(16, 8), CNONCE));
        expectCode("aead", evil, E2e.Session.host(keys).seal("nope"));
    }

    @Test
    public void pairingPayloadValidation() throws Exception {
        String ok = "{\"v\":1,\"host\":\"ab12\",\"ep\":[\"ws://10.0.2.2:18797\",\"wss://x.ngrok-free.app\"],\"dev\":\"0123456789abcdef\","
                + "\"psk\":\"" + Crypto.b64encode(PSK) + "\",\"chal\":\"" + Crypto.b64encode(filled(16, 1)) + "\",\"exp\":1900000000}";
        E2e.Pairing p = E2e.parsePairing("payload: " + ok);
        assertEquals(2, p.endpoints.size());
        assertEquals("0123456789abcdef", p.deviceId);
        assertArrayEquals(PSK, p.psk);
        String[][] bad = {
            {"malformed", "nope"},
            {"unsupported-version", ok.replace("\"v\":1", "\"v\":2")},
            {"missing-device-id", ok.replace("\"dev\":\"0123456789abcdef\",", "")},
            {"malformed", ok.replace("0123456789abcdef", "0123456789ABCDEF")},
            {"bad-endpoint", ok.replace("ws://10.0.2.2:18797", "http://x")},
            {"bad-psk", ok.replace(Crypto.b64encode(PSK), "AAAA")},
            {"bad-expiry", ok.replace("1900000000", "-1")},
        };
        for (String[] b : bad) {
            try {
                E2e.parsePairing(b[1]);
                fail("must reject " + b[1]);
            } catch (E2e.E2eException e) {
                assertEquals(b[1], b[0], e.code);
            }
        }
    }
}
