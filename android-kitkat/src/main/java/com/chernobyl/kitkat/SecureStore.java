package com.chernobyl.kitkat;

import android.content.Context;
import android.content.SharedPreferences;
import android.security.KeyPairGeneratorSpec;
import java.math.BigInteger;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.UnrecoverableKeyException;
import java.util.Calendar;
import javax.crypto.BadPaddingException;
import javax.crypto.Cipher;
import javax.crypto.IllegalBlockSizeException;
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

    /**
     * True when a failed unwrap can never succeed: the wrap key is gone or was replaced
     * (bad padding / block size, unrecoverable key, permanently invalidated key). False for a
     * Keystore that is only unavailable for now (locked, busy, provider error): nothing may be
     * wiped then. Looks through the cause chain.
     */
    static boolean unrecoverable(Throwable e) {
        for (int depth = 0; e != null && depth < 5; depth++, e = e.getCause()) {
            if (e instanceof BadPaddingException || e instanceof IllegalBlockSizeException
                    || e instanceof UnrecoverableKeyException) return true;
            for (Class<?> c = e.getClass(); c != null; c = c.getSuperclass()) {
                if (c.getName().equals("android.security.keystore.KeyPermanentlyInvalidatedException")) return true;
            }
        }
        return false;
    }

    /** The data key, or null while the Keystore cannot be used right now (retried on the next call). */
    private synchronized byte[] key() {
        if (dataKey != null) return dataKey;
        String wrapped = prefs.getString("dk", null), plain = prefs.getString("dk_plain", null);
        if (wrapped != null) {
            try {
                dataKey = rsa(Cipher.DECRYPT_MODE, Crypto.b64decode(wrapped));
                mode = "Android Keystore (RSA-wrapped key)";
                if (plain != null) prefs.edit().remove("dk_plain").commit(); // a leftover from before the wrap worked
                return dataKey;
            } catch (Exception e) {
                if (!unrecoverable(e)) return null;
                // the Keystore lost the wrap key (lock screen reset): stored values are unreadable, start over
                prefs.edit().clear().commit();
                plain = null;
            }
        }
        byte[] stored = plain == null ? null : Crypto.b64decode(plain);
        if (stored != null) {
            // the Keystore refused last time: wrap the key now, and drop the plain copy once that worked
            try {
                prefs.edit().putString("dk", Crypto.b64encode(rsa(Cipher.ENCRYPT_MODE, stored))).remove("dk_plain").commit();
                mode = "Android Keystore (RSA-wrapped key)";
            } catch (Exception e) {
                mode = "app-private storage (no Keystore)";
            }
            dataKey = stored;
            return stored;
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
        byte[] key = key();
        if (key == null) return null;
        byte[] nonce = new byte[12], sealed = new byte[b.length - 12];
        System.arraycopy(b, 0, nonce, 0, 12);
        System.arraycopy(b, 12, sealed, 0, sealed.length);
        byte[] plain = Crypto.open(key, nonce, Crypto.utf8(name), sealed);
        return plain == null ? null : Crypto.utf8Decode(plain);
    }

    /** Synchronous (commit): callers rely on it being on disk, e.g. before acking a rekey. */
    boolean put(String name, String value) {
        if (value == null) return prefs.edit().remove("v_" + name).commit();
        byte[] key = key();
        if (key == null) return false;
        byte[] nonce = Crypto.random(12);
        byte[] sealed = Crypto.seal(key, nonce, Crypto.utf8(name), Crypto.utf8(value));
        return prefs.edit().putString("v_" + name, Crypto.b64encode(Crypto.concat(nonce, sealed))).commit();
    }
}
