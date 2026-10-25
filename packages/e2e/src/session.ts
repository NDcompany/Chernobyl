/**
 * Sealed frames (ADR-011 wire contract): every frame after `hs_ok` is
 *
 *   {"n": <counter>, "d": "<base64 ChaCha20-Poly1305(ciphertext ‖ 16B tag)>"}
 *
 * with per-direction HKDF keys, the AEAD nonce = the 8-byte big-endian counter
 * zero-padded to 12 bytes, and a strict per-direction counter (replay and
 * desync both throw without advancing state) — byte-for-byte the semantics of
 * services/companion-rs/src/e2e.rs.
 */
import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { hkdf } from '@noble/hashes/hkdf.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { b64decode, b64encode } from './base64.js';
import { E2eError } from './errors.js';
import { utf8Decode, utf8Encode } from './utf8.js';

export interface SessionKeys {
  c2h: Uint8Array;
  h2c: Uint8Array;
}

/** PRK = HMAC(salt, psk); OKM = HKDF-Expand(PRK, info, 32) — RFC 5869. */
function hkdf32(psk: Uint8Array, salt: Uint8Array, info: string): Uint8Array {
  const infoBytes = utf8Encode(info);
  return hkdf(sha256, psk, salt, infoBytes, 32);
}

/** Per-connection directional keys from PSK + the handshake's fresh salt. */
export function deriveKeys(psk: Uint8Array, salt: Uint8Array): SessionKeys {
  return {
    c2h: hkdf32(psk, salt, 'chernobyl-e2e-c2h'),
    h2c: hkdf32(psk, salt, 'chernobyl-e2e-h2c'),
  };
}

/**
 * Bytes 0-7 = big-endian counter, bytes 8-11 = zero — byte-exact with
 * e2e.rs `counter_nonce`. (Counter 0 is all-zero in any layout, so only a
 * counter > 0 vector catches a mismatch: see session.test.ts.)
 */
export function counterNonce(counter: number): Uint8Array {
  const nonce = new Uint8Array(12);
  let c = counter;
  for (let i = 7; i >= 0; i -= 1) {
    nonce[i] = c % 256;
    c = Math.floor(c / 256);
  }
  return nonce;
}

/** One endpoint of a sealed connection: seals with its send key, opens with its receive key. */
export class SealedSession {
  private readonly sendKey: Uint8Array;
  private readonly recvKey: Uint8Array;
  /** counter for the next frame this endpoint seals (AEAD nonce). */
  private tx = 0;
  /** expected counter of the next frame from the peer. */
  private rx = 0;

  constructor(sendKey: Uint8Array, recvKey: Uint8Array) {
    this.sendKey = sendKey;
    this.recvKey = recvKey;
  }

  /** Client endpoint: seals client→host (c2h), opens host→client (h2c). */
  static client(keys: SessionKeys): SealedSession {
    return new SealedSession(keys.c2h, keys.h2c);
  }

  /** Host endpoint: seals host→client (h2c), opens client→host (c2h). */
  static host(keys: SessionKeys): SealedSession {
    return new SealedSession(keys.h2c, keys.c2h);
  }

  /** Seal a plaintext into the transport envelope. */
  seal(plaintext: string): string {
    const nonce = counterNonce(this.tx);
    const sealed = chacha20poly1305(this.sendKey, nonce).encrypt(utf8Encode(plaintext));
    const envelope = `{"n":${this.tx},"d":"${b64encode(sealed)}"}`;
    this.tx += 1;
    return envelope;
  }

  /** Open a transport envelope. Throws `E2eError` on replay/desync/tamper/garbage. */
  open(envelope: string): string {
    let v: unknown;
    try {
      v = JSON.parse(envelope);
    } catch {
      throw new E2eError('malformed', 'malformed sealed envelope');
    }
    const o =
      typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
    if (!o || typeof o.n !== 'number' || !Number.isSafeInteger(o.n) || o.n < 0) {
      throw new E2eError('malformed', 'missing or invalid counter');
    }
    if (typeof o.d !== 'string') {
      throw new E2eError('malformed', 'missing body');
    }
    if (o.n < this.rx) {
      throw new E2eError('replay', `frame replay (counter ${o.n} < expected ${this.rx})`);
    }
    if (o.n > this.rx) {
      throw new E2eError('desync', `counter desync (counter ${o.n} > expected ${this.rx})`);
    }
    const sealed = b64decode(o.d);
    if (sealed === null) {
      throw new E2eError('malformed', 'bad base64 body');
    }
    let plain: Uint8Array;
    try {
      plain = chacha20poly1305(this.recvKey, counterNonce(o.n)).decrypt(sealed);
    } catch {
      throw new E2eError('aead', 'AEAD authentication failed');
    }
    const text = utf8Decode(plain);
    if (text === null) {
      throw new E2eError('bad-utf8', 'sealed body not valid UTF-8');
    }
    this.rx += 1;
    return text;
  }
}
