/**
 * Pairing payload parsing — the QR is a trust boundary (it carries the PSK),
 * so every field is validated and every rejection path is pinned.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { PairingError, b64encode, parsePairingPayload } from './index.js';

function payload(overrides: Record<string, unknown> = {}): string {
  return JSON.stringify({
    v: 1,
    host: '0123456789abcdef',
    ep: ['ws://127.0.0.1:8787'],
    dev: 'aabbccddeeff0011',
    psk: b64encode(new Uint8Array(32).fill(7)),
    chal: b64encode(new Uint8Array(16).fill(9)),
    exp: 1_800_000_000,
    ...overrides,
  });
}

test('valid payload parses to exact fields (pairing.rs payload shape)', () => {
  const c = parsePairingPayload(payload());
  assert.equal(c.host, '0123456789abcdef');
  assert.deepEqual(c.endpoints, ['ws://127.0.0.1:8787']);
  assert.equal(c.deviceId, 'aabbccddeeff0011');
  assert.equal(c.psk.length, 32);
  assert.equal(c.psk[0], 7);
  assert.equal(c.challenge.length, 16);
  assert.equal(c.expiresAtSec, 1_800_000_000);
});

test('wss endpoints are accepted, multiple endpoints kept in order', () => {
  const c = parsePairingPayload(
    payload({ ep: ['ws://192.168.1.5:8787', 'wss://host.example:443/path'] }),
  );
  assert.deepEqual(c.endpoints, ['ws://192.168.1.5:8787', 'wss://host.example:443/path']);
});

function expectCode(code: string, fn: () => unknown): void {
  try {
    fn();
  } catch (err) {
    assert.ok(err instanceof PairingError, `expected PairingError, got ${String(err)}`);
    assert.equal(err.code, code);
    return;
  }
  assert.fail(`expected PairingError(${code}) but nothing was thrown`);
}

test('malformed payloads are rejected with precise codes', () => {
  expectCode('malformed', () => parsePairingPayload('not json'));
  expectCode('malformed', () => parsePairingPayload('[1,2,3]'));
  expectCode('unsupported-version', () => parsePairingPayload(payload({ v: 2 })));
  expectCode('unsupported-version', () => parsePairingPayload(payload({ v: '1' })));
  expectCode('malformed', () => parsePairingPayload(payload({ host: '' })));
  expectCode('malformed', () => parsePairingPayload(payload({ host: 42 })));
  expectCode('bad-endpoint', () => parsePairingPayload(payload({ ep: [] })));
  expectCode('bad-endpoint', () => parsePairingPayload(payload({ ep: ['http://nope'] })));
  expectCode('bad-endpoint', () => parsePairingPayload(payload({ ep: ['ws://a b'] })));
  expectCode('missing-device-id', () => parsePairingPayload(payload({ dev: undefined })));
  expectCode('malformed', () => parsePairingPayload(payload({ dev: 'AABB' })));
  expectCode('malformed', () => parsePairingPayload(payload({ dev: 'aabbccddeeff001Z' })));
  expectCode('bad-psk', () => parsePairingPayload(payload({ psk: b64encode(new Uint8Array(16)) })));
  expectCode('bad-psk', () => parsePairingPayload(payload({ psk: '!!' })));
  expectCode('bad-psk', () => parsePairingPayload(payload({ chal: b64encode(new Uint8Array(15)) })));
  expectCode('bad-expiry', () => parsePairingPayload(payload({ exp: 1.5 })));
  expectCode('bad-expiry', () => parsePairingPayload(payload({ exp: -1 })));
  expectCode('bad-expiry', () => parsePairingPayload(payload({ exp: 'soon' })));
  expectCode('bad-expiry', () => parsePairingPayload(payload({ exp: 1e20 })));
});
