package com.chernobyl.kitkat;

import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertTrue;

import android.security.keystore.KeyPermanentlyInvalidatedException;
import java.io.IOException;
import java.security.KeyStoreException;
import java.security.ProviderException;
import java.security.UnrecoverableKeyException;
import javax.crypto.BadPaddingException;
import javax.crypto.IllegalBlockSizeException;
import org.junit.Test;

public class SecureStoreTest {
    @Test
    public void aLostOrReplacedWrapKeyIsUnrecoverable() {
        assertTrue(SecureStore.unrecoverable(new BadPaddingException()));
        assertTrue(SecureStore.unrecoverable(new IllegalBlockSizeException()));
        assertTrue(SecureStore.unrecoverable(new UnrecoverableKeyException()));
        // wrapped in another exception
        assertTrue(SecureStore.unrecoverable(new RuntimeException("x", new BadPaddingException())));
        // found by class name, so the main code needs no API 23 class
        assertTrue(SecureStore.unrecoverable(new KeyPermanentlyInvalidatedException()));
    }

    @Test
    public void aKeystoreThatIsOnlyBusyOrLockedIsNotWiped() {
        assertFalse(SecureStore.unrecoverable(new KeyStoreException("locked")));
        assertFalse(SecureStore.unrecoverable(new ProviderException("keystore busy")));
        assertFalse(SecureStore.unrecoverable(new IOException("io")));
        assertFalse(SecureStore.unrecoverable(new RuntimeException()));
        assertFalse(SecureStore.unrecoverable(null));
    }
}
