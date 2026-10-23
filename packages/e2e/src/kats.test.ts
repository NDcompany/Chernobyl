/**
 * Known-answer tests for the exact primitives and constructions the wire
 * contract uses — the same RFC/NIST vectors services/companion-rs/src/crypto.rs
 * and e2e.rs assert, so any divergence between the TS and Rust stacks fails
 * here before an interop run is even attempted.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { sha256 } from '@noble/hashes/sha2.js';
import { hmac } from '@noble/hashes/hmac.js';
import { hkdf } from '@noble/hashes/hkdf.js';
import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { deriveKeys, handshakeProof, proofOk, utf8Decode, utf8Encode } from './index.js';
import { b64decode, b64encode } from './base64.js';

const hex = (b: Uint8Array): string => Buffer.from(b).toString('hex');

test('sha256 matches FIPS 180-4 vectors (crypto.rs sha256_vectors)', () => {
  assert.equal(hex(sha256(new Uint8Array(0))), 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
  assert.equal(hex(sha256(utf8Encode('abc'))), 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
  assert.equal(
    hex(sha256(utf8Encode('abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq'))),
    '248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1',
  );
});

test('hmac-sha256 matches RFC 4231 vectors (crypto.rs hmac_sha256_rfc4231)', () => {
  assert.equal(
    hex(hmac(sha256, new Uint8Array(20).fill(0x0b), utf8Encode('Hi There'))),
    'b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7',
  );
  assert.equal(
    hex(hmac(sha256, utf8Encode('Jefe'), utf8Encode('what do ya want for nothing?'))),
    '5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843',
  );
  assert.equal(
    hex(hmac(sha256, new Uint8Array(20).fill(0xaa), new Uint8Array(50).fill(0xdd))),
    '773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe',
  );
  const k4 = new Uint8Array(25);
  for (let i = 0; i < 25; i += 1) k4[i] = i + 1;
  assert.equal(
    hex(hmac(sha256, k4, new Uint8Array(50).fill(0xcd))),
    '82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b',
  );
});

test('hkdf matches RFC 5869 case 1 (crypto.rs hkdf_rfc5869_case1)', () => {
  const ikm = new Uint8Array(22).fill(0x0b);
  const salt = new Uint8Array([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
  const info = new Uint8Array([0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9]);
  const prk = hmac(sha256, salt, ikm);
  assert.equal(hex(prk), '077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5');
  assert.equal(
    hex(hkdf(sha256, ikm, salt, info, 42)),
    '3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865',
  );
});

test('ChaCha20-Poly1305 AEAD matches RFC 8439 2.8.2 (crypto.rs aead_rfc8439_vector)', () => {
  const key = new Uint8Array(32);
  for (let i = 0; i < 32; i += 1) key[i] = 0x80 + i;
  const nonce = new Uint8Array([7, 0, 0, 0, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47]);
  const aad = new Uint8Array([0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7]);
  const plaintext = utf8Encode(
    "Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.",
  );
  // noble's chacha20poly1305 takes aad at construction; the contract uses none,
  // but the RFC vector exercises the same core with aad - proving parity.
  const sealed = chacha20poly1305(key, nonce, aad).encrypt(plaintext);
  assert.equal(hex(sealed.subarray(sealed.length - 16)), '1ae10b594f09e26a7e902ecbd0600691');
  assert.equal(hex(chacha20poly1305(key, nonce, aad).decrypt(sealed)), hex(plaintext));
});

test('handshake_proof binds role, device id, nonce and salt (e2e.rs proof_binds)', () => {
  const psk = new Uint8Array(32).fill(7);
  const nonce = utf8Encode('NONCENONCENONCE');
  const salt = utf8Encode('SALTSALTSALTSALT');
  const a = handshakeProof(psk, 'client', 'dev1', nonce, salt);
  assert.notEqual(hex(a), hex(handshakeProof(psk, 'server', 'dev1', nonce, salt)));
  assert.notEqual(hex(a), hex(handshakeProof(psk, 'client', 'dev2', nonce, salt)));
  assert.notEqual(hex(a), hex(handshakeProof(psk, 'client', 'dev1', utf8Encode('OTHERNONCEOTHER'), salt)));
  assert.notEqual(hex(a), hex(handshakeProof(psk, 'client', 'dev1', nonce, utf8Encode('OTHERSALTOTHERS'))));
  assert.notEqual(hex(a), hex(handshakeProof(new Uint8Array(32).fill(8), 'client', 'dev1', nonce, salt)));
  assert.ok(proofOk(a, a));
  const wrong = a.slice();
  wrong[0] ^= 1;
  assert.ok(!proofOk(a, wrong));
  assert.ok(!proofOk(a, a.subarray(0, 16)), 'length mismatch must fail');
});

test('derive_keys is deterministic and directional (e2e.rs keys_are_deterministic)', () => {
  const psk = new Uint8Array(32).fill(7);
  const salt = new Uint8Array(16).fill(9);
  const a = deriveKeys(psk, salt);
  const b = deriveKeys(psk, salt);
  assert.equal(hex(a.c2h), hex(b.c2h));
  assert.equal(hex(a.h2c), hex(b.h2c));
  assert.notEqual(hex(a.c2h), hex(a.h2c), 'directions must not share a key');
  assert.equal(a.c2h.length, 32);
  assert.notEqual(hex(a.c2h), hex(deriveKeys(psk, new Uint8Array(16).fill(8)).c2h));
});

test('base64 round-trips and rejects malformed input', () => {
  assert.equal(b64encode(new Uint8Array([0, 0])), 'AAA=');
  assert.equal(b64encode(new Uint8Array([0xfb, 0xff])), '+/8=');
  assert.equal(b64encode(new Uint8Array([1, 2, 3, 4, 5])), 'AQIDBAU=');
  const bytes = new Uint8Array(256);
  for (let i = 0; i < 256; i += 1) bytes[i] = i;
  assert.equal(hex(b64decode(b64encode(bytes)) ?? new Uint8Array()), hex(bytes));
  assert.equal(b64decode('!!'), null, 'bad alphabet');
  assert.equal(b64decode('AAAAA'), null, 'impossible length');
  assert.equal(b64decode('AA=A'), null, 'padding in the middle');
  assert.equal(b64decode('AAB='), null, 'non-zero trailing bits');
  assert.ok(b64decode('AQIDBAU'), 'unpadded input is accepted');
});

test('utf8 matches Rust String::from_utf8 semantics', () => {
  const s = 'cůžek 日本語 🚀 tail';
  assert.equal(utf8Decode(utf8Encode(s)), s);
  assert.equal(utf8Decode(new Uint8Array([0x00, 0x7f])), String.fromCharCode(0, 0x7f));
  const bad: Array<[string, number[]]> = [
    ['lone continuation', [0x80, 0x81]],
    ['overlong 2-byte', [0xc0, 0x80]],
    ['overlong slash', [0xe0, 0x80, 0xaf]],
    ['truncated 3-byte', [0xe2, 0x82]],
    ['truncated 4-byte', [0xf0, 0x9f, 0x9a]],
    ['encoded surrogate', [0xed, 0xa0, 0x80]],
    ['beyond U+10FFFF', [0xf4, 0x90, 0x80, 0x80]],
    ['0xff', [0xff]],
  ];
  for (const [name, arr] of bad) {
    assert.equal(utf8Decode(new Uint8Array(arr)), null, name);
  }
});
