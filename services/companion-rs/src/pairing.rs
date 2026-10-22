//! Pairing (ADR-002/011): single-use, short-TTL, rate-limited offers that issue
//! a per-device 256-bit PSK, delivered out-of-band as a terminal QR.
//!
//! `companion pair` creates an offer, persists it (PSK DPAPI-protected) so the
//! running daemon can redeem it, and prints the QR. Redemption (the E2E session
//! layer) marks it single-use, checks TTL, verifies the phone's PSK proof, and
//! records the paired device. The QR is the only place the plaintext PSK leaves
//! the process — it is scanned at physical proximity (out-of-band channel).

use crate::base64;
use crate::ffi;
use crate::json::{self, Json};

/// How long a pairing offer stays valid (ADR-011: short TTL, ~5 minutes).
pub const PAIRING_TTL_MS: i64 = 5 * 60 * 1000;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A pending pairing offer: single-use, short-lived, carries the PSK that the
/// phone must prove knowledge of during the E2E handshake.
pub struct PairingOffer {
    pub device_id: String,
    pub psk: [u8; 32],
    pub challenge: [u8; 16],
    pub created_at: i64,
    pub expires_at: i64,
    consumed: bool,
}

impl PairingOffer {
    /// Fresh offer: random device id, 256-bit PSK, 128-bit challenge, TTL now.
    pub fn new(now: i64) -> Result<Self, String> {
        let id = ffi::csprng_bytes(8)?;
        let psk: [u8; 32] = ffi::csprng_bytes(32)?
            .try_into()
            .map_err(|_| "PSK must be 32 bytes".to_string())?;
        let challenge: [u8; 16] = ffi::csprng_bytes(16)?
            .try_into()
            .map_err(|_| "challenge must be 16 bytes".to_string())?;
        Ok(Self {
            device_id: hex(&id),
            psk,
            challenge,
            created_at: now,
            expires_at: now + PAIRING_TTL_MS,
            consumed: false,
        })
    }

    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    pub fn is_consumed(&self) -> bool {
        self.consumed
    }

    /// Single-use: first consume succeeds; any later attempt fails. Called at
    /// redemption so one QR can never pair two devices.
    pub fn consume(&mut self) -> Result<(), String> {
        if self.consumed {
            return Err("pairing offer already used".to_string());
        }
        self.consumed = true;
        Ok(())
    }

    /// The QR payload the phone scans (versioned compact JSON).
    pub fn payload_json(&self, host_id: &str, endpoints: &[String]) -> String {
        let ep = endpoints.iter().map(|e| json::s(e)).collect();
        json::stringify(&json::obj(vec![
            ("v", Json::Int(1)),
            ("host", json::s(host_id)),
            ("ep", Json::Arr(ep)),
            ("psk", json::s(&base64::encode(&self.psk))),
            ("chal", json::s(&base64::encode(&self.challenge))),
            ("exp", Json::Int(self.expires_at / 1000)),
        ]))
    }
}

/// Bounds pairing attempts to `max` offers per `window_ms` (ADR-011:
/// rate-limited pairing). Sliding window of offer-creation timestamps.
pub struct RateLimiter {
    max: usize,
    window_ms: i64,
    stamps: Vec<i64>,
}

impl RateLimiter {
    pub fn new(max: usize, window_ms: i64) -> Self {
        Self {
            max,
            window_ms,
            stamps: Vec::new(),
        }
    }

    /// Records `now` if under the cap and returns true; false when throttled
    /// (window not yet drained).
    pub fn allow(&mut self, now: i64) -> bool {
        self.stamps.retain(|&t| now - t < self.window_ms);
        if self.stamps.len() >= self.max {
            return false;
        }
        self.stamps.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_generates_unique_ids_and_valid_shape() {
        let a = PairingOffer::new(1_000).unwrap();
        let b = PairingOffer::new(1_000).unwrap();
        assert_ne!(a.device_id, b.device_id, "device ids must be random");
        assert_eq!(a.device_id.len(), 16, "8 bytes hex -> 16 chars");
        // PSK must actually look random (not all zero)
        assert!(a.psk.iter().any(|&x| x != 0));
        assert_eq!(a.expires_at - a.created_at, PAIRING_TTL_MS);
    }

    #[test]
    fn ttl_expiry_is_checked() {
        let mut o = PairingOffer::new(0).unwrap();
        assert!(!o.is_expired(PAIRING_TTL_MS - 1));
        assert!(o.is_expired(PAIRING_TTL_MS));
        o.consume().unwrap();
    }

    #[test]
    fn consume_is_single_use() {
        let mut o = PairingOffer::new(0).unwrap();
        assert!(!o.is_consumed());
        o.consume().unwrap();
        assert!(o.is_consumed());
        assert!(o.consume().is_err(), "second consume must fail");
    }

    #[test]
    fn payload_is_parseable_versioned_json() {
        let o = PairingOffer::new(0).unwrap();
        let eps = vec!["ws://192.168.1.5:8787".to_string()];
        let payload = o.payload_json("host-abc", &eps);
        let v = json::parse(&payload).expect("payload is valid JSON");
        assert_eq!(json::get_i64(&v, "v"), Some(1));
        assert_eq!(json::get_str(&v, "host"), Some("host-abc"));
        // PSK in payload is base64 of the exact 32 bytes
        let psk = json::get_str(&v, "psk").unwrap();
        assert_eq!(base64::decode(psk).unwrap(), o.psk);
        let chal = json::get_str(&v, "chal").unwrap();
        assert_eq!(base64::decode(chal).unwrap(), o.challenge);
        // endpoints array round-trips
        let arr = json::get(&v, "ep").and_then(json::as_arr).unwrap();
        assert_eq!(json::as_str(&arr[0]), Some("ws://192.168.1.5:8787"));
        // exp is unix seconds = expires_at / 1000
        assert_eq!(json::get_i64(&v, "exp"), Some(o.expires_at / 1000));
    }

    #[test]
    fn rate_limiter_throttles_beyond_cap_and_recovers() {
        let mut rl = RateLimiter::new(3, 1_000);
        assert!(rl.allow(0));
        assert!(rl.allow(100));
        assert!(rl.allow(200));
        assert!(!rl.allow(300), "4th within window is throttled");
        // once the whole window has drained, allow opens up again
        assert!(rl.allow(1_200), "window has elapsed");
    }

    #[test]
    fn payload_fits_qr_v10_capacity() {
        // A realistic payload (2 endpoints) must not exceed what the QR encoder
        // can carry, or `companion pair` would print nothing.
        let o = PairingOffer::new(0).unwrap();
        let eps = vec![
            "ws://192.168.100.23:8787".to_string(),
            "ws://10.0.0.4:8787".to_string(),
        ];
        let payload = o.payload_json("0123456789abcdef", &eps);
        let qr = crate::qr::encode(payload.as_bytes()).expect("payload fits QR");
        assert!(qr.size >= 21);
    }
}
