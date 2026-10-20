//! Base64 (RFC 4648) encoder + strict decoder. Used for the RFC 6455
//! handshake key/accept (base64 of 16/20 raw bytes) and nothing else.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Strict decode: correct alphabet, correct padding, no trailing bits.
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.as_chunks::<4>().0 {
        let pad = match chunk[3] {
            b'=' => {
                if chunk[2] == b'=' {
                    2
                } else {
                    1
                }
            }
            _ => 0,
        };
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' if i >= 4 - pad => 0, // padding only in final positions
                _ => return None,
            } as u32;
            n = (n << 6) | v;
        }
        // strictness: bits of the final sextets beyond the decoded bytes
        // must be zero (e.g. "Zm9=" has garbage in the 2 unused bits)
        let unused: u32 = ((4 - pad) * 6 - (3 - pad) * 8) as u32;
        if unused > 0 && (n & (((1u32 << unused) - 1) << (6 * pad))) != 0 {
            return None;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn round_trip() {
        for data in [
            &b""[..],
            &b"\x00"[..],
            &[0xff; 16][..],
            &[0xff; 20][..],
            b"hello world!",
        ] {
            assert_eq!(decode(&encode(data)).unwrap(), data);
        }
    }

    #[test]
    fn strict_decode_rejects() {
        assert_eq!(decode("A"), None); // bad length
        assert_eq!(decode("AAAA="), None); // bad padding length
        assert_eq!(decode("AA=A"), None); // '=' in the middle
        assert_eq!(decode("Zm9="), None); // non-zero trailing bits (pad=1)
        assert_eq!(decode("A==="), None);
        assert_eq!(decode("!!!!"), None); // bad alphabet
        // no-padding input has no trailing bits to check: valid
        assert_eq!(decode("AAAB").unwrap(), [0, 0, 1]);
    }

    #[test]
    fn ws_key_decodes_to_16_bytes() {
        let raw = decode("dGhlIHNhbXBsZSBub25jZQ==").unwrap();
        assert_eq!(raw.len(), 16);
    }
}
