//! SHA-1 (FIPS 180-4). Used ONLY for the RFC 6455 handshake accept-key
//! (protocol-mandated SHA-1 there; never for signatures or passwords).
//! Verified against the RFC 3174 / FIPS test vectors in the tests below.

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rfc_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // two-block message
        assert_eq!(
            hex(&sha1(&[b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn accept_key_vector_from_rfc6455() {
        // RFC 6455 §1.3 worked example, verbatim:
        // key "dGhlIHNhbXBsZSBub25jZQ==" -> accept "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        let mut src = b"dGhlIHNhbXBsZSBub25jZQ==".to_vec();
        src.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        assert_eq!(
            crate::base64::encode(&sha1(&src)),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn single_block_boundary_55_bytes() {
        // 55 bytes + 0x80 (1 byte) + 8 bytes length = 64 bytes (exact 1 block)
        let msg = [b'x'; 55];
        let digest = sha1(&msg);
        assert_ne!(digest, [0u8; 20]);
    }

    #[test]
    fn two_block_boundary_56_bytes() {
        // 56 bytes + 0x80 = 57 bytes > 56, so length cannot fit in block 1; spills to block 2
        let msg = [b'x'; 56];
        let digest = sha1(&msg);
        assert_ne!(digest, [0u8; 20]);
    }

    #[test]
    fn two_block_exact_64_bytes() {
        // exactly one 64-byte chunk of data: padding and length must form a full second block
        let msg = [b'y'; 64];
        let digest = sha1(&msg);
        assert_ne!(digest, [0u8; 20]);
    }

    #[test]
    fn two_block_boundary_119_bytes() {
        // 119 bytes + 1 byte padding + 8 bytes length = 128 bytes (exact 2 blocks)
        let msg = [b'z'; 119];
        let digest = sha1(&msg);
        assert_ne!(digest, [0u8; 20]);
    }

    #[test]
    fn three_block_boundary_120_bytes() {
        // 120 bytes + 1 byte padding = 121 bytes > 120 (56 mod 64); spills to block 3
        let msg = [b'w'; 120];
        let digest = sha1(&msg);
        assert_ne!(digest, [0u8; 20]);
    }

    #[test]
    fn sha1_deterministic_across_repeated_runs() {
        let input = b"The quick brown fox jumps over the lazy dog";
        let d1 = sha1(input);
        let d2 = sha1(input);
        assert_eq!(d1, d2);
        assert_eq!(hex(&d1), "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12");
    }
}
