/**
 * Base64 (RFC 4648, standard alphabet) for the E2E wire contract — hand-rolled
 * because the mobile runtime has no `Buffer` and the contract's base64 must be
 * byte-identical with the Rust companion's `base64.rs` on both sides.
 */

const ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

// Reverse table: -1 = invalid, -2 = '=' padding.
const LOOKUP = (() => {
  const t = new Int8Array(128).fill(-1);
  for (let i = 0; i < ALPHABET.length; i += 1) t[ALPHABET.charCodeAt(i)] = i;
  t['='.charCodeAt(0)] = -2;
  return t;
})();

/** Standard base64 with padding (matches Rust `base64::encode`). */
export function b64encode(bytes: Uint8Array): string {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i];
    const b1 = i + 1 < bytes.length ? bytes[i + 1] : 0;
    const b2 = i + 2 < bytes.length ? bytes[i + 2] : 0;
    out += ALPHABET[b0 >> 2];
    out += ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)];
    out += i + 1 < bytes.length ? ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)] : '=';
    out += i + 2 < bytes.length ? ALPHABET[b2 & 0x3f] : '=';
  }
  return out;
}

/**
 * Strict decode: returns null on any malformed input (bad alphabet, bad
 * length, non-zero trailing bits, stray padding) — callers map that to an
 * explicit wire error instead of guessing.
 */
export function b64decode(text: string): Uint8Array | null {
  let s = text;
  while (s.endsWith('=')) s = s.slice(0, -1);
  if (s.length % 4 === 1) return null;
  const out = new Uint8Array(Math.floor((s.length * 3) / 4));
  let acc = 0;
  let bits = 0;
  let o = 0;
  for (let i = 0; i < s.length; i += 1) {
    const c = s.charCodeAt(i);
    const v = c < 128 ? LOOKUP[c] : -1;
    if (v < 0) return null;
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out[o] = (acc >> bits) & 0xff;
      o += 1;
    }
  }
  // bits%8 === 4 or 2 (leftover) must be zero — rejects "AB=="-style junk.
  if (bits > 0 && (acc & ((1 << bits) - 1)) !== 0) return null;
  return out.subarray(0, o);
}
