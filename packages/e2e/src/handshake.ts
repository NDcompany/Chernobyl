/**
 * The `hs_*` handshake (ADR-011/012 wire contract):
 *
 *   C→S {"type":"hs_hello","deviceId":"<hex>","cnonce":"<b64 16B>"}
 *   S→C {"type":"hs_challenge","nonce":"<b64 16B>","salt":"<b64 16B>"}
 *   C→S {"type":"hs_proof","proof":"<b64 32B>"}
 *   S→C {"type":"hs_ok","serverProof":"<b64 32B>"} | {"type":"hs_err","reason":"…"}
 *
 * with proof = HMAC-SHA256(psk, role ‖ 0x00 ‖ deviceId ‖ 0x00 ‖ nonce ‖ 0x00 ‖ salt ‖ 0x00 ‖ cnonce)
 * on both sides (mutual authentication — a MITM without the PSK cannot produce
 * either proof). `cnonce` is fresh per attempt from the OS CSPRNG, so a
 * recorded server transcript replayed to a new attempt fails as
 * `server-proof-mismatch` and never re-derives an old session's keys
 * (security review 2026-10-25). The PSK never leaves the process; the
 * handshake proves knowledge of it. Byte-identical with
 * services/companion-rs/src/e2e.rs.
 */
import { hmac } from '@noble/hashes/hmac.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { concatBytes, randomBytes } from '@noble/hashes/utils.js';
import { b64decode, b64encode } from './base64.js';
import { E2eError } from './errors.js';
import type { PairingCredential } from './pairing.js';
import { SealedSession, deriveKeys } from './session.js';
import { utf8Encode } from './utf8.js';

/**
 * HMAC binding role + device id + the server's fresh nonce/salt + our fresh
 * client nonce to the PSK. Same construction both roles use; the server
 * verifies ours, we verify its.
 */
export function handshakeProof(
  psk: Uint8Array,
  role: 'client' | 'server',
  deviceId: string,
  nonce: Uint8Array,
  salt: Uint8Array,
  cnonce: Uint8Array,
): Uint8Array {
  const z = new Uint8Array(1);
  return hmac(sha256, psk, concatBytes(utf8Encode(role), z, utf8Encode(deviceId), z, nonce, z, salt, z, cnonce));
}

/** Constant-time proof comparison (never short-circuits on the first diff). */
export function proofOk(expected: Uint8Array, got: Uint8Array): boolean {
  if (expected.length !== got.length) return false;
  let diff = 0;
  for (let i = 0; i < expected.length; i += 1) diff |= expected[i] ^ got[i];
  return diff === 0;
}

/** Transport the handshake runs over (one connection's raw text frames). */
export interface HandshakeIo {
  send(text: string): void;
  /** Next text frame from the server. */
  recv(): Promise<string>;
}

function readJson(text: string): Record<string, unknown> {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new E2eError('malformed', 'handshake frame is not JSON');
  }
  if (typeof v !== 'object' || v === null || Array.isArray(v)) {
    throw new E2eError('malformed', 'handshake frame must be a JSON object');
  }
  return v as Record<string, unknown>;
}

async function withTimeout(p: Promise<string>, ms: number, what: string): Promise<string> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      p,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new E2eError('handshake-timeout', `timed out waiting for ${what}`)), ms);
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

function expectFixedB64(frame: Record<string, unknown>, field: string, len: number): Uint8Array {
  const raw = frame[field];
  const bytes = typeof raw === 'string' ? b64decode(raw) : null;
  if (!bytes || bytes.length !== len) {
    throw new E2eError('malformed', `${field} must be base64 of ${len} bytes`);
  }
  return bytes;
}

function throwIfErr(frame: Record<string, unknown>): void {
  if (frame.type === 'hs_err') {
    const reason = typeof frame.reason === 'string' ? frame.reason : 'no reason given';
    throw new E2eError('handshake-rejected', reason);
  }
}

/**
 * Run one full handshake and return the established session (client side).
 * Throws `E2eError` with code `handshake-rejected` on hs_err (unknown, spent,
 * expired or revoked device), `server-proof-mismatch` when the server cannot
 * prove PSK knowledge (possible MITM), `malformed`/`handshake-timeout` on bad
 * or missing frames.
 */
export async function performHandshake(
  io: HandshakeIo,
  pairing: PairingCredential,
  opts?: { timeoutMs?: number },
): Promise<SealedSession> {
  const timeoutMs = opts?.timeoutMs ?? 10_000;

  // OS CSPRNG (crypto.getRandomValues); throws rather than fall back to a
  // weak source when the runtime has none
  const cnonce = randomBytes(16);
  io.send(JSON.stringify({ type: 'hs_hello', deviceId: pairing.deviceId, cnonce: b64encode(cnonce) }));

  const challenge = readJson(
    await withTimeout(io.recv(), timeoutMs, 'hs_challenge'),
  );
  throwIfErr(challenge);
  if (challenge.type !== 'hs_challenge') {
    throw new E2eError('malformed', `expected hs_challenge, got ${JSON.stringify(challenge.type)}`);
  }
  const nonce = expectFixedB64(challenge, 'nonce', 16);
  const salt = expectFixedB64(challenge, 'salt', 16);

  const proof = handshakeProof(pairing.psk, 'client', pairing.deviceId, nonce, salt, cnonce);
  io.send(JSON.stringify({ type: 'hs_proof', proof: b64encode(proof) }));

  const ok = readJson(await withTimeout(io.recv(), timeoutMs, 'hs_ok'));
  throwIfErr(ok);
  if (ok.type !== 'hs_ok') {
    throw new E2eError('malformed', `expected hs_ok, got ${JSON.stringify(ok.type)}`);
  }
  const serverProof = expectFixedB64(ok, 'serverProof', 32);
  const expected = handshakeProof(pairing.psk, 'server', pairing.deviceId, nonce, salt, cnonce);
  if (!proofOk(expected, serverProof)) {
    throw new E2eError('server-proof-mismatch', 'server failed mutual authentication');
  }

  return SealedSession.client(deriveKeys(pairing.psk, salt, cnonce));
}
