//! E2E session layer (ADR-011): PSK handshake, HKDF directional keys, and
//! ChaCha20-Poly1305 frame sealing with monotonic counters (replay-proof).
//!
//! Security model: the per-device 256-bit PSK arrives only via the out-of-band
//! QR (physical proximity). A fresh server nonce binds every handshake — a
//! captured proof cannot be replayed against a new connection, because the proof
//! is an HMAC over that connection's server nonce + salt. HKDF derives one key
//! per direction, and the AEAD nonce is the frame's monotonic counter, so every
//! sealed frame is tamper-proof (auth tag) and replay-proof (strict counter).
//!
//! No X25519 forward secrecy (documented future work, ADR-011 §"future"): PSK +
//! fresh nonces. Post-compromise secrecy is explicitly not claimed — if the PSK
//! leaks, past sessions are only safe if the attacker also recorded traffic and
//! the PSK is later rotated.

use crate::base64;
use crate::crypto::{aead_open, aead_seal, ct_eq, hkdf_expand, hkdf_extract, hmac_sha256};
use crate::ffi;
use crate::json;

/// A per-device 256-bit pre-shared key (from a pairing offer).
pub type Psk = [u8; 32];

/// Directional keys for one connection, derived from the PSK + a fresh salt.
#[derive(Clone)]
pub struct SessionKeys {
    c2h: Psk,
    h2c: Psk,
}

fn hkdf32(psk: &Psk, salt: &[u8], info: &[u8]) -> Psk {
    let prk = hkdf_extract(salt, psk);
    let mut okm = [0u8; 32];
    hkdf_expand(&prk, info, &mut okm);
    okm
}

/// Derive per-connection directional keys from the PSK and a fresh random salt.
/// `c2h` seals client→host frames, `h2c` seals host→client frames.
pub fn derive_keys(psk: &Psk, salt: &[u8]) -> SessionKeys {
    SessionKeys {
        c2h: hkdf32(psk, salt, b"chernobyl-e2e-c2h"),
        h2c: hkdf32(psk, salt, b"chernobyl-e2e-h2c"),
    }
}

/// HMAC proof binding a role + device id + the server's fresh nonce/salt to the
/// PSK. Both sides compute it; constant-time equality proves mutual PSK
/// knowledge without the PSK ever leaving the process.
pub fn handshake_proof(
    psk: &Psk,
    role: &str,
    device_id: &str,
    nonce: &[u8],
    salt: &[u8],
) -> [u8; 32] {
    let mut msg = Vec::new();
    msg.extend_from_slice(role.as_bytes());
    msg.push(0);
    msg.extend_from_slice(device_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(nonce);
    msg.push(0);
    msg.extend_from_slice(salt);
    hmac_sha256(psk, &msg)
}

/// Constant-time proof check (never compares lengths/timing on the raw bytes).
pub fn proof_ok(expected: &[u8; 32], got: &[u8]) -> bool {
    ct_eq(expected, got)
}

/// Fresh 16-byte server nonce + 16-byte HKDF salt for one handshake.
pub fn fresh_nonce_salt() -> ([u8; 16], [u8; 16]) {
    let nonce = ffi::csprng_bytes(16)
        .expect("CSPRNG")
        .try_into()
        .expect("16 bytes");
    let salt = ffi::csprng_bytes(16)
        .expect("CSPRNG")
        .try_into()
        .expect("16 bytes");
    (nonce, salt)
}

/// AEAD nonce for a frame counter: 8-byte big-endian counter, zero-padded to the
/// 12-byte ChaCha20 nonce. One counter per direction ⇒ a never-reused nonce.
fn counter_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// A live E2E session over one connection. Each endpoint seals with its send key
/// and opens with its receive key; counters are strict per direction.
pub struct Session {
    send: Psk,
    recv: Psk,
    /// counter for the next frame this endpoint seals (AEAD nonce).
    tx: u64,
    /// expected counter of the next frame from the peer.
    rx: u64,
}

impl Session {
    /// Host endpoint: seals host→client (h2c), opens client→host (c2h).
    pub fn host(keys: SessionKeys) -> Self {
        Session {
            send: keys.h2c,
            recv: keys.c2h,
            tx: 0,
            rx: 0,
        }
    }

    /// Client endpoint: seals client→host (c2h), opens host→client (h2c).
    pub fn client(keys: SessionKeys) -> Self {
        Session {
            send: keys.c2h,
            recv: keys.h2c,
            tx: 0,
            rx: 0,
        }
    }

    /// Seal a plaintext into a transport payload: JSON `{"n":counter,"d":b64}`.
    pub fn seal(&mut self, plaintext: &str) -> String {
        let nonce = counter_nonce(self.tx);
        let sealed = aead_seal(&self.send, &nonce, &[], plaintext.as_bytes());
        let payload = format!(
            "{{\"n\":{},\"d\":\"{}\"}}",
            self.tx,
            base64::encode(&sealed)
        );
        self.tx += 1;
        payload
    }

    /// Open a transport payload from the peer. Rejects replay (counter already
    /// seen) and desync (counter skipped); a tampered body fails the AEAD tag.
    pub fn open(&mut self, payload: &str) -> Result<String, String> {
        let v = json::parse(payload).map_err(|_| "malformed sealed envelope".to_string())?;
        let n = json::get_i64(&v, "n").ok_or("missing counter")?;
        let d = json::get_str(&v, "d").ok_or("missing body")?;
        if n < 0 {
            return Err("negative counter".to_string());
        }
        let n = n as u64;
        if n < self.rx {
            return Err(format!("frame replay (counter {n} < expected {})", self.rx));
        }
        if n > self.rx {
            return Err(format!(
                "counter desync (counter {n} > expected {})",
                self.rx
            ));
        }
        let sealed = base64::decode(d).ok_or_else(|| "bad base64 body".to_string())?;
        let nonce = counter_nonce(n);
        let plain =
            aead_open(&self.recv, &nonce, &[], &sealed).ok_or("AEAD authentication failed")?;
        let text = String::from_utf8(plain).map_err(|_| "sealed body not UTF-8".to_string())?;
        self.rx += 1;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psk() -> Psk {
        [7u8; 32]
    }

    #[test]
    fn keys_are_deterministic_and_directional() {
        let salt = [9u8; 16];
        let a = derive_keys(&psk(), &salt);
        let b = derive_keys(&psk(), &salt);
        // same inputs ⇒ same keys, and the two directions must differ
        assert_eq!(a.c2h, b.c2h);
        assert_eq!(a.h2c, b.h2c);
        assert_ne!(a.c2h, a.h2c, "directions must not share a key");
        // a different salt yields different keys (per-connection freshness)
        let c = derive_keys(&psk(), &[8u8; 16]);
        assert_ne!(a.c2h, c.c2h);
    }

    #[test]
    fn proof_binds_role_device_and_nonce() {
        let psk = psk();
        let (nonce, salt) = (
            b"NONCENONCENONCE".as_slice(),
            b"SALTSALTSALTSALT".as_slice(),
        );
        let a = handshake_proof(&psk, "client", "dev1", nonce, salt);
        // any change to role / device / nonce / salt changes the proof
        assert_ne!(a, handshake_proof(&psk, "server", "dev1", nonce, salt));
        assert_ne!(a, handshake_proof(&psk, "client", "dev2", nonce, salt));
        assert_ne!(
            a,
            handshake_proof(&psk, "client", "dev1", b"OTHERNONCEOTHER", salt)
        );
        assert_ne!(
            a,
            handshake_proof(&psk, "client", "dev1", nonce, b"OTHERSALTOTHERS")
        );
        // a different PSK cannot forge the same proof
        assert_ne!(
            a,
            handshake_proof(&[8u8; 32], "client", "dev1", nonce, salt)
        );
        // proof_ok does constant-time equality
        assert!(proof_ok(&a, &a));
        let mut wrong = a;
        wrong[0] ^= 1;
        assert!(!proof_ok(&a, &wrong));
    }

    #[test]
    fn seal_open_round_trips_and_counts_up() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT");
        let mut host = Session::host(keys.clone());
        let mut peer = Session::client(keys);
        // host → peer
        let f0 = host.seal("hello");
        assert_eq!(peer.open(&f0).unwrap(), "hello");
        let f1 = host.seal("world");
        assert_eq!(peer.open(&f1).unwrap(), "world");
        // peer → host (other direction uses the other key/counter)
        let r0 = peer.seal("ack");
        assert_eq!(host.open(&r0).unwrap(), "ack");
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT");
        let mut host = Session::host(keys.clone());
        let mut peer = Session::client(keys);
        let f0 = peer.seal("once");
        let f1 = peer.seal("twice");
        assert_eq!(host.open(&f0).unwrap(), "once");
        assert_eq!(host.open(&f1).unwrap(), "twice");
        // replaying an already-accepted frame must fail
        assert!(host.open(&f0).unwrap_err().contains("replay"));
    }

    #[test]
    fn skipped_counter_is_desync() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT");
        let mut host = Session::host(keys.clone());
        let mut peer = Session::client(keys);
        let _ = peer.seal("a"); // counter 0
        let _ = peer.seal("b"); // counter 1
        // host expects 0 but is handed counter 1 (a skip) ⇒ desync
        let f1 = peer.seal("c");
        assert!(host.open(&f1).unwrap_err().contains("desync"));
    }

    #[test]
    fn tampered_body_fails_auth_tag() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT");
        let mut host = Session::host(keys.clone());
        let mut peer = Session::client(keys);
        let frame = peer.seal("secret message");
        // flip one base64 char in the body (not the counter) ⇒ AEAD tag fails
        let mut tampered = frame.clone();
        let idx = tampered.rfind('\"').unwrap() - 1;
        let c = tampered.as_bytes()[idx];
        tampered.replace_range(idx..idx + 1, if c == b'A' { "B" } else { "A" });
        assert!(host.open(&tampered).is_err());
    }

    #[test]
    fn open_rejects_garbage_envelope() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT");
        let mut host = Session::host(keys);
        assert!(host.open("not json").is_err());
        assert!(host.open("{\"n\":0}").is_err()); // missing body
        assert!(host.open("{\"d\":\"AAAA\"}").is_err()); // missing counter
    }

    #[test]
    fn fresh_nonce_salt_are_random() {
        let (a, b) = fresh_nonce_salt();
        assert_ne!(a, b, "nonce must be fresh");
    }
}
