//! E2E session layer (ADR-011): PSK handshake, HKDF directional keys, and
//! ChaCha20-Poly1305 frame sealing with monotonic counters (replay-proof).
//!
//! Security model: the per-device 256-bit PSK arrives only via the out-of-band
//! QR (physical proximity). Fresh nonces from BOTH sides bind every handshake:
//! the server's nonce + salt stop a captured client proof from being replayed
//! against a new connection, and the client's nonce (`cnonce`) stops a recorded
//! server transcript from authenticating an impostor host or re-deriving an old
//! session's keys, which would repeat AEAD key+nonce pairs (security review
//! 2026-10-25). HKDF derives one key per direction, and the AEAD nonce is the
//! frame's monotonic counter, so every sealed frame is tamper-proof (auth tag)
//! and replay-proof (strict counter).
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

/// Directional keys for one connection, derived from the PSK + the handshake's
/// fresh server salt and client nonce.
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

/// Derive per-connection directional keys from the PSK, the server's fresh salt
/// and the client's fresh nonce: HKDF salt = `salt ‖ cnonce`, so a replayed
/// server salt alone never reproduces an earlier session's keys.
/// `c2h` seals client→host frames, `h2c` seals host→client frames.
pub fn derive_keys(psk: &Psk, salt: &[u8], cnonce: &[u8]) -> SessionKeys {
    let salt = [salt, cnonce].concat();
    SessionKeys {
        c2h: hkdf32(psk, &salt, b"chernobyl-e2e-c2h"),
        h2c: hkdf32(psk, &salt, b"chernobyl-e2e-h2c"),
    }
}

/// HMAC proof binding a role + device id + the server's fresh nonce/salt + the
/// client's fresh nonce to the PSK. Both sides compute it; constant-time
/// equality proves mutual PSK knowledge without the PSK ever leaving the process.
pub fn handshake_proof(
    psk: &Psk,
    role: &str,
    device_id: &str,
    nonce: &[u8],
    salt: &[u8],
    cnonce: &[u8],
) -> [u8; 32] {
    let mut msg = Vec::new();
    msg.extend_from_slice(role.as_bytes());
    msg.push(0);
    msg.extend_from_slice(device_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(nonce);
    msg.push(0);
    msg.extend_from_slice(salt);
    msg.push(0);
    msg.extend_from_slice(cnonce);
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

    /// Client nonce the fixed vectors use (16 bytes, like the salt).
    const CNONCE: &[u8] = b"CNONCNONCNONCNON";

    #[test]
    fn keys_are_deterministic_and_directional() {
        let salt = [9u8; 16];
        let a = derive_keys(&psk(), &salt, CNONCE);
        let b = derive_keys(&psk(), &salt, CNONCE);
        // same inputs ⇒ same keys, and the two directions must differ
        assert_eq!(a.c2h, b.c2h);
        assert_eq!(a.h2c, b.h2c);
        assert_ne!(a.c2h, a.h2c, "directions must not share a key");
        // a different salt yields different keys (per-connection freshness)
        let c = derive_keys(&psk(), &[8u8; 16], CNONCE);
        assert_ne!(a.c2h, c.c2h);
    }

    /// Security review 2026-10-25: a replayed server transcript repeats the
    /// salt, so the client's fresh nonce alone must change both keys — else
    /// the phone re-derives a recorded session's keys and reuses its
    /// ChaCha20-Poly1305 key+nonce pairs from counter 0.
    #[test]
    fn same_server_salt_with_a_new_client_nonce_gives_new_keys() {
        let salt = [9u8; 16];
        let a = derive_keys(&psk(), &salt, &[1u8; 16]);
        let b = derive_keys(&psk(), &salt, &[2u8; 16]);
        assert_ne!(a.c2h, b.c2h, "c2h must depend on the client nonce");
        assert_ne!(a.h2c, b.h2c, "h2c must depend on the client nonce");
    }

    #[test]
    fn proof_binds_role_device_and_nonce() {
        let psk = psk();
        let (nonce, salt) = (
            b"NONCENONCENONCE".as_slice(),
            b"SALTSALTSALTSALT".as_slice(),
        );
        let a = handshake_proof(&psk, "client", "dev1", nonce, salt, CNONCE);
        // any change to role / device / nonce / salt / cnonce changes the proof
        assert_ne!(
            a,
            handshake_proof(&psk, "server", "dev1", nonce, salt, CNONCE)
        );
        assert_ne!(
            a,
            handshake_proof(&psk, "client", "dev2", nonce, salt, CNONCE)
        );
        assert_ne!(
            a,
            handshake_proof(&psk, "client", "dev1", b"OTHERNONCEOTHER", salt, CNONCE)
        );
        assert_ne!(
            a,
            handshake_proof(&psk, "client", "dev1", nonce, b"OTHERSALTOTHERS", CNONCE)
        );
        // a recorded server proof does not verify for a new client nonce
        assert_ne!(
            handshake_proof(&psk, "server", "dev1", nonce, salt, CNONCE),
            handshake_proof(&psk, "server", "dev1", nonce, salt, b"OTHERCNONCEOTHER")
        );
        // a different PSK cannot forge the same proof
        assert_ne!(
            a,
            handshake_proof(&[8u8; 32], "client", "dev1", nonce, salt, CNONCE)
        );
        // proof_ok does constant-time equality
        assert!(proof_ok(&a, &a));
        let mut wrong = a;
        wrong[0] ^= 1;
        assert!(!proof_ok(&a, &wrong));
    }

    /// Cross-language golden vectors, pinned byte-for-byte in
    /// packages/e2e/src/session.test.ts. Counter 0 is an all-zero nonce in
    /// any layout, so the n=1 frames are what pin the nonce byte order — a
    /// client that put the counter in bytes 4-11 broke every phone session
    /// on its second frame while every counter-0 check still passed.
    #[test]
    fn golden_sealed_frames_match_the_ts_client() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT", CNONCE);
        let mut client = Session::client(keys.clone());
        let mut host = Session::host(keys);
        assert_eq!(
            client.seal("hello"),
            r#"{"n":0,"d":"r6Rs7v9M2/y/CAkkvJJ+xqHQUhv8"}"#
        );
        assert_eq!(
            client.seal("ping"),
            r#"{"n":1,"d":"HSVsxvDYQ7qYlBmBoyBF2XmxHNw="}"#
        );
        assert_eq!(
            host.seal("hello_ack"),
            r#"{"n":0,"d":"zLVPpIF5s4rcEesgzm6PwVXf3OuKDblSmQ=="}"#
        );
        assert_eq!(
            host.seal("pong"),
            r#"{"n":1,"d":"e6r062GVTFNYUGpBRQ0Ue4hoGLI="}"#
        );
    }

    #[test]
    fn seal_open_round_trips_and_counts_up() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT", CNONCE);
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
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT", CNONCE);
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
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT", CNONCE);
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
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT", CNONCE);
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
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT", CNONCE);
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

    #[test]
    fn rejected_envelopes_leave_rx_intact_and_next_frame_still_opens() {
        let keys = derive_keys(&psk(), b"SALTSALTSALTSALT", CNONCE);
        let mut host = Session::host(keys.clone());
        let mut peer = Session::client(keys);
        let f0 = peer.seal("legit-0");

        // hostile envelopes: each rejected without advancing rx or touching tx
        let hostile = [
            "not json",
            "",
            "{\"n\":0}",                                   // missing body
            "{\"d\":\"AAAA\"}",                            // missing counter
            "{\"n\":-1,\"d\":\"AAAA\"}",                   // negative counter
            "{\"n\":1.5,\"d\":\"AAAA\"}",                  // float counter is not an int
            "{\"n\":99999999999999999999,\"d\":\"AAAA\"}", // beyond i64
            "{\"n\":0,\"d\":\"!!\"}",                      // invalid base64
            "{\"n\":0,\"d\":\"AAAA\"}",                    // decodes, fails AEAD
        ];
        for env in &hostile {
            let err = host.open(env).unwrap_err();
            assert!(!err.is_empty(), "must reject with a reason: {env}");
            assert_eq!(host.rx, 0, "rx must not advance on: {env} (got {err})");
        }
        assert_eq!(host.tx, 0, "rejected opens must not touch the send counter");

        // the next legitimate frame still opens: no state was corrupted
        assert_eq!(host.open(&f0).unwrap(), "legit-0");
        assert_eq!(host.rx, 1);
        // replay of the accepted frame: rejected, rx stays 1
        assert!(host.open(&f0).unwrap_err().contains("replay"));
        assert_eq!(host.rx, 1);

        // future counter is desync, not corruption: rx stays, then in-order works
        let f1 = peer.seal("legit-1");
        let f2 = peer.seal("legit-2");
        assert!(host.open(&f2).unwrap_err().contains("desync"));
        assert_eq!(host.rx, 1);
        assert_eq!(host.open(&f1).unwrap(), "legit-1");

        // tampered body at the expected counter: AEAD rejection, rx intact
        let mut bad = f2.clone();
        let start = bad.find("\"d\":\"").unwrap() + 5; // first body char
        let c = bad.as_bytes()[start];
        let repl = if c == b'A' { b'B' } else { b'A' };
        bad.replace_range(start..start + 1, &(repl as char).to_string());
        assert!(host.open(&bad).unwrap_err().contains("AEAD"));
        assert_eq!(host.rx, 2);
        // the untampered frame still opens afterward
        assert_eq!(host.open(&f2).unwrap(), "legit-2");
        assert_eq!(host.rx, 3);
    }
}
