/**
 * Sealed-frame semantics — mirrors services/companion-rs/src/e2e.rs tests
 * one-for-one (round-trip, replay, desync, tamper, hostile envelopes leaving
 * rx intact) so both stacks enforce the same contract.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { SealedSession, deriveKeys, E2eError, utf8Encode } from './index.js';

const psk = new Uint8Array(32).fill(7);
const salt = utf8Encode('SALTSALTSALTSALT'); // exactly 16 bytes

function pair(): { host: SealedSession; client: SealedSession } {
  const keys = deriveKeys(psk, salt);
  return { host: SealedSession.host(keys), client: SealedSession.client(keys) };
}

function expectCode(code: string, fn: () => unknown): void {
  try {
    fn();
  } catch (err) {
    assert.ok(err instanceof E2eError, `expected E2eError, got ${String(err)}`);
    assert.equal(err.code, code);
    return;
  }
  assert.fail(`expected E2eError(${code}) but nothing was thrown`);
}

test('seal/open round-trips both directions with counters counting up', () => {
  const { host, client } = pair();
  const f0 = host.seal('hello');
  assert.match(f0, /^\{"n":0,"d":"[A-Za-z0-9+/=]+"\}$/);
  assert.equal(client.open(f0), 'hello');
  assert.equal(client.open(host.seal('world')), 'world');
  assert.equal(host.open(client.seal('ack')), 'ack');
  // unicode survives the full AEAD + UTF-8 path
  const s = 'žluťoučký 🚀 kůň';
  assert.equal(host.open(client.seal(s)), s);
});

test('replayed frame is rejected (e2e.rs replayed_frame_is_rejected)', () => {
  const { host, client } = pair();
  const f0 = client.seal('once');
  const f1 = client.seal('twice');
  assert.equal(host.open(f0), 'once');
  assert.equal(host.open(f1), 'twice');
  expectCode('replay', () => host.open(f0));
});

test('skipped counter is desync (e2e.rs skipped_counter_is_desync)', () => {
  const { host, client } = pair();
  client.seal('a'); // counter 0
  client.seal('b'); // counter 1
  const f2 = client.seal('c'); // counter 2
  expectCode('desync', () => host.open(f2));
});

test('tampered body fails the auth tag (e2e.rs tampered_body_fails_auth_tag)', () => {
  const { host, client } = pair();
  const frame = client.seal('secret message');
  const i = frame.lastIndexOf('"');
  const at = i - 1;
  const c = frame.charCodeAt(at);
  const flipped = frame.slice(0, at) + (c === 65 ? 'B' : 'A') + frame.slice(at + 1);
  expectCode('aead', () => host.open(flipped));
});

test('open rejects garbage envelopes (e2e.rs open_rejects_garbage_envelope)', () => {
  const { host } = pair();
  expectCode('malformed', () => host.open('not json'));
  expectCode('malformed', () => host.open('{"n":0}'));
  expectCode('malformed', () => host.open('{"d":"AAAA"}'));
});

test('hostile envelopes leave rx intact and the next frame still opens', () => {
  const { host, client } = pair();
  const f0 = client.seal('legit-0');
  const hostile = [
    'not json',
    '',
    '{"n":0}',
    '{"d":"AAAA"}',
    '{"n":-1,"d":"AAAA"}',
    '{"n":1.5,"d":"AAAA"}',
    '{"n":99999999999999999999,"d":"AAAA"}', // beyond safe integer == Rust's i64 bound
    '{"n":0,"d":"!!"}',
    '{"n":0,"d":"AAAA"}', // decodes, fails AEAD
  ];
  for (const env of hostile) {
    try {
      host.open(env);
      assert.fail(`must reject: ${env}`);
    } catch (err) {
      assert.ok(err instanceof E2eError, `${env}: expected E2eError, got ${String(err)}`);
      assert.ok(err.code.length > 0);
    }
  }
  assert.equal(host.open(f0), 'legit-0');
  expectCode('replay', () => host.open(f0));
  const f1 = client.seal('legit-1');
  const f2 = client.seal('legit-2');
  expectCode('desync', () => host.open(f2));
  assert.equal(host.open(f1), 'legit-1');
  assert.equal(host.open(f2), 'legit-2');
});

test('directional keys: client seal opens on host and vice versa', () => {
  const keys = deriveKeys(psk, salt);
  const host = SealedSession.host(keys);
  const client = SealedSession.client(keys);
  assert.equal(host.open(client.seal('c->h')), 'c->h');
  assert.equal(client.open(host.seal('h->c')), 'h->c');
  // a session with wrong keys cannot open a fresh frame (tag check fails)
  const evil = SealedSession.client(deriveKeys(psk, new Uint8Array(16).fill(8)));
  expectCode('aead', () => evil.open(SealedSession.host(keys).seal('nope')));
});
