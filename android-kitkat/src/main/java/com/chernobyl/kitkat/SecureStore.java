package com.chernobyl.kitkat;

import android.content.Context;
import android.content.SharedPreferences;
import android.security.KeyPairGeneratorSpec;
import java.math.BigInteger;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.util.Calendar;
import javax.crypto.Cipher;
import javax.security.auth.x500.X500Principal;

/**
 * At-rest storage for the pairing (device id + PSK) and the access token.
 * Values are sealed with ChaCha20-Poly1305 (Crypto) under a random 32-byte data
 * key; the data key is wrapped by an RSA key pair in AndroidKeyStore (API 18+;
 * KitKat has no Keystore AES). If the Keystore refuses (some 4.4 builds, or after
 * the lock screen was reset), the data key falls back to app-private
 * SharedPreferences, readable only by this app's uid: mode() says which.
 */
final class SecureStore {
    private static final String ALIAS = "chernobyl-kitkat-wrap";
    private final SharedPreferences prefs;
    private final Context ctx;
    private byte[] dataKey;
    private String mode = "none";

    SecureStore(Context ctx) {
        this.ctx = ctx;
        prefs = ctx.getSharedPreferences("secure", Context.MODE_PRIVATE);
    }

    /** "Android Keystore (RSA-wrapped key)" or "app-private storage (no Keystore)". */
    String mode() {
        key();
        return mode;
    }

    private synchronized byte[] key() {
        if (dataKey != null) return dataKey;
        String wrapped = prefs.getString("dk", null), plain = prefs.getString("dk_plain", null);
        try {
            if (wrapped != null) {
                dataKey = rsa(Cipher.DECRYPT_MODE, Crypto.b64decode(wrapped));
                mode = "Android Keystore (RSA-wrapped key)";
                return dataKey;
            }
        } catch (Exception e) {
            // the Keystore lost the wrap key (lock screen reset): stored values are unreadable, start over
            prefs.edit().clear().commit();
        }
        if (plain != null) {
            dataKey = Crypto.b64decode(plain);
            mode = "app-private storage (no Keystore)";
            return dataKey;
        }
        byte[] k = Crypto.random(32);
        try {
            prefs.edit().putString("dk", Crypto.b64encode(rsa(Cipher.ENCRYPT_MODE, k))).commit();
            mode = "Android Keystore (RSA-wrapped key)";
        } catch (Exception e) {
            prefs.edit().putString("dk_plain", Crypto.b64encode(k)).commit();
            mode = "app-private storage (no Keystore)";
        }
        dataKey = k;
        return k;
    }

    private byte[] rsa(int op, byte[] data) throws Exception {
        KeyStore ks = KeyStore.getInstance("AndroidKeyStore");
        ks.load(null);
        if (!ks.containsAlias(ALIAS)) {
            Calendar from = Calendar.getInstance(), to = Calendar.getInstance();
            to.add(Calendar.YEAR, 30);
            @SuppressWarnings("deprecation")
            KeyPairGeneratorSpec spec = new KeyPairGeneratorSpec.Builder(ctx).setAlias(ALIAS)
                    .setSubject(new X500Principal("CN=" + ALIAS)).setSerialNumber(BigInteger.ONE)
                    .setStartDate(from.getTime()).setEndDate(to.getTime()).build();
            KeyPairGenerator g = KeyPairGenerator.getInstance("RSA", "AndroidKeyStore");
            g.initialize(spec);
            g.generateKeyPair();
        }
        Cipher c = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        if (op == Cipher.ENCRYPT_MODE) c.init(op, (PublicKey) ks.getCertificate(ALIAS).getPublicKey());
        else c.init(op, (PrivateKey) ks.getKey(ALIAS, null));
        return c.doFinal(data);
    }

    String get(String name) {
        String v = prefs.getString("v_" + name, null);
        if (v == null) return null;
        byte[] b = Crypto.b64decode(v);
        if (b == null || b.length < 28) return null;
        byte[] nonce = new byte[12], sealed = new byte[b.length - 12];
        System.arraycopy(b, 0, nonce, 0, 12);
        System.arraycopy(b, 12, sealed, 0, sealed.length);
        byte[] plain = Crypto.open(key(), nonce, Crypto.utf8(name), sealed);
        return plain == null ? null : Crypto.utf8Decode(plain);
    }

    /** Synchronous (commit): callers rely on it being on disk, e.g. before acking a rekey. */
    boolean put(String name, String value) {
        if (value == null) return prefs.edit().remove("v_" + name).commit();
        byte[] nonce = Crypto.random(12);
        byte[] sealed = Crypto.seal(key(), nonce, Crypto.utf8(name), Crypto.utf8(value));
        return prefs.edit().putString("v_" + name, Crypto.b64encode(Crypto.concat(nonce, sealed))).commit();
    }
}
