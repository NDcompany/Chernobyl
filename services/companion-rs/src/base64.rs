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
    let chunks = bytes.as_chunks::<4>().0;
    for (k, chunk) in chunks.iter().enumerate() {
        // RFC 4648: '=' only in the final group ("Zg==Zg==" is refused)
        let pad = match chunk[3] {
            _ if k + 1 < chunks.len() => 0,
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

    #[test]
    fn empty_buffer_decodes_to_empty_vec() {
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn single_byte_padding_two_equals() {
        assert_eq!(encode(&[0x41]), "QQ==");
        assert_eq!(decode("QQ==").unwrap(), vec![0x41]);
    }

    #[test]
    fn two_bytes_padding_one_equal() {
        assert_eq!(encode(&[0x41, 0x42]), "QUI=");
        assert_eq!(decode("QUI=").unwrap(), vec![0x41, 0x42]);
    }

    #[test]
    fn three_bytes_no_padding() {
        assert_eq!(encode(&[0x41, 0x42, 0x43]), "QUJD");
        assert_eq!(decode("QUJD").unwrap(), vec![0x41, 0x42, 0x43]);
    }

    #[test]
    fn strict_decode_rejects_whitespace_and_newlines() {
        assert_eq!(decode("Zm9v\n"), None);
        assert_eq!(decode(" Zm9v"), None);
        assert_eq!(decode("Zm 9v"), None);
        assert_eq!(decode("Zm9v\r\n"), None);
    }

    #[test]
    fn strict_decode_rejects_urlsafe_substitutions() {
        assert_eq!(decode("Zm-v"), None);
        assert_eq!(decode("Zm_v"), None);
    }

    #[test]
    fn strict_decode_rejects_non_zero_trailing_bits_pad_two() {
        // "ZE==" has non-zero bits in the unused 4 bits of the second char
        assert_eq!(decode("ZE=="), None);
        // "Zg==" decodes cleanly to 'f'
        assert_eq!(decode("Zg==").unwrap(), b"f");
    }

    // RFC 4648: '=' may only end the whole string, not an inner group
    #[test]
    fn strict_decode_rejects_embedded_padding_in_multiblock() {
        // padding '=' can only appear at the end of the entire string
        assert_eq!(decode("Zg==Zg=="), None);
        assert_eq!(decode("QUI=QUI="), None);
    }

    #[test]
    fn all_byte_values_round_trip() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }
}
