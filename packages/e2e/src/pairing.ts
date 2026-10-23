/**
 * Pairing payload (the QR the phone scans) — parsing and strict validation.
 *
 * Shape is the frozen contract emitted by `PairingOffer::payload_json`
 * (services/companion-rs/src/pairing.rs), documented in ADR-012:
 *
 *   {"v":1,"host":"<hex host id>","ep":["ws://host:port", …],
 *    "dev":"<16-hex device id>","psk":"<b64 32B>","chal":"<b64 16B>",
 *    "exp": <unix seconds>}
 *
 * `dev` is mandatory: the server redeems the offer by the hs_hello deviceId, so
 * a payload without it cannot pair (that is the contract defect ADR-012 fixed).
 */
import { b64decode } from './base64.js';

export interface PairingCredential {
  host: string;
  endpoints: string[];
  deviceId: string;
  psk: Uint8Array;
  challenge: Uint8Array;
  /** Offer expiry, unix SECONDS (server `expires_at / 1000`). */
  expiresAtSec: number;
}

/** Typed parse failure with a stable `code` the UI can branch on. */
export class PairingError extends Error {
  readonly code:
    | 'malformed'
    | 'unsupported-version'
    | 'missing-device-id'
    | 'bad-psk'
    | 'bad-endpoint'
    | 'bad-expiry';

  constructor(code: PairingError['code'], message: string) {
    super(message);
    this.name = 'PairingError';
    this.code = code;
  }
}

function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

/**
 * Parse + validate a QR payload. Throws `PairingError` with a precise reason —
 * never returns a partially valid credential.
 */
export function parsePairingPayload(raw: string): PairingCredential {
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    throw new PairingError('malformed', 'pairing payload is not valid JSON');
  }
  const o = asRecord(v);
  if (!o) throw new PairingError('malformed', 'pairing payload must be a JSON object');

  if (o.v !== 1) {
    throw new PairingError(
      'unsupported-version',
      `unsupported pairing payload version: ${JSON.stringify(o.v)}`,
    );
  }

  if (typeof o.host !== 'string' || o.host.length === 0) {
    throw new PairingError('malformed', 'payload must carry a host id (host)');
  }

  if (!Array.isArray(o.ep) || o.ep.length === 0) {
    throw new PairingError('bad-endpoint', 'payload must carry at least one endpoint (ep)');
  }
  for (const e of o.ep) {
    if (typeof e !== 'string' || !/^wss?:\/\/[^\s]+$/.test(e)) {
      throw new PairingError('bad-endpoint', `endpoint is not a ws(s) URL: ${JSON.stringify(e)}`);
    }
  }

  if (typeof o.dev !== 'string') {
    throw new PairingError(
      'missing-device-id',
      'payload must carry the device id (dev) — see ADR-012',
    );
  }
  if (!/^[0-9a-f]{16}$/.test(o.dev)) {
    throw new PairingError('malformed', 'device id (dev) must be 16 lowercase hex characters');
  }

  const psk = typeof o.psk === 'string' ? b64decode(o.psk) : null;
  if (!psk || psk.length !== 32) {
    throw new PairingError('bad-psk', 'psk must be base64 of exactly 32 bytes');
  }

  const challenge = typeof o.chal === 'string' ? b64decode(o.chal) : null;
  if (!challenge || challenge.length !== 16) {
    throw new PairingError('bad-psk', 'chal must be base64 of exactly 16 bytes');
  }

  if (typeof o.exp !== 'number' || !Number.isSafeInteger(o.exp) || o.exp <= 0) {
    throw new PairingError('bad-expiry', 'exp must be a positive unix-seconds integer');
  }

  return {
    host: o.host,
    endpoints: o.ep as string[],
    deviceId: o.dev,
    psk,
    challenge,
    expiresAtSec: o.exp,
  };
}
