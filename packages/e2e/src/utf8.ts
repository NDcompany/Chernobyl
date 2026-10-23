/**
 * UTF-8 for sealed-frame bodies. Encoding reuses noble's audited `utf8ToBytes`;
 * decoding is a strict hand-rolled walker (byte-identical with Rust
 * `String::from_utf8` semantics: invalid sequences are an error, never a
 * replacement character), because the mobile runtime does not guarantee a
 * global `TextDecoder`.
 */
import { utf8ToBytes } from '@noble/hashes/utils.js';

export const utf8Encode = utf8ToBytes;

/** Strict UTF-8 decode; returns null on any invalid byte sequence. */
export function utf8Decode(bytes: Uint8Array): string | null {
  let out = '';
  let i = 0;
  const n = bytes.length;
  while (i < n) {
    const b0 = bytes[i];
    if (b0 < 0x80) {
      out += String.fromCharCode(b0);
      i += 1;
      continue;
    }
    let cp: number;
    let extra: number;
    let min: number;
    if (b0 >= 0xc2 && b0 <= 0xdf) {
      cp = b0 & 0x1f;
      extra = 1;
      min = 0x80;
    } else if (b0 >= 0xe0 && b0 <= 0xef) {
      cp = b0 & 0x0f;
      extra = 2;
      min = 0x800;
    } else if (b0 >= 0xf0 && b0 <= 0xf4) {
      cp = b0 & 0x07;
      extra = 3;
      min = 0x10000;
    } else {
      return null; // continuation byte as lead, or 0xc0/0xc1/0xf5..0xff
    }
    if (i + extra >= n) return null; // truncated multi-byte sequence
    for (let k = 1; k <= extra; k += 1) {
      const bk = bytes[i + k];
      if ((bk & 0xc0) !== 0x80) return null;
      cp = (cp << 6) | (bk & 0x3f);
    }
    if (cp < min || cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) return null;
    if (cp < 0x10000) {
      out += String.fromCharCode(cp);
    } else {
      const adj = cp - 0x10000;
      out += String.fromCharCode(0xd800 + (adj >> 10), 0xdc00 + (adj & 0x3ff));
    }
    i += extra + 1;
  }
  return out;
}
