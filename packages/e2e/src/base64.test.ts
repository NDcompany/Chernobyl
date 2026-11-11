import assert from 'node:assert/strict';
import test from 'node:test';
import { b64decode, b64encode } from './base64.js';

test('base64: RFC 4648 standard test vectors', () => {
  const enc = (s: string) => b64encode(new TextEncoder().encode(s));
  const dec = (s: string) => new TextDecoder().decode(b64decode(s)!);

  assert.equal(enc(''), '');
  assert.equal(enc('f'), 'Zg==');
  assert.equal(enc('fo'), 'Zm8=');
  assert.equal(enc('foo'), 'Zm9v');
  assert.equal(enc('foob'), 'Zm9vYg==');
  assert.equal(enc('fooba'), 'Zm9vYmE=');
  assert.equal(enc('foobar'), 'Zm9vYmFy');

  assert.equal(dec(''), '');
  assert.equal(dec('Zg=='), 'f');
  assert.equal(dec('Zm8='), 'fo');
  assert.equal(dec('Zm9v'), 'foo');
  assert.equal(dec('Zm9vYg=='), 'foob');
  assert.equal(dec('Zm9vYmE='), 'fooba');
  assert.equal(dec('Zm9vYmFy'), 'foobar');
});

test('base64: round-trip on binary buffers of all remainder lengths', () => {
  for (let len = 0; len <= 32; len++) {
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) bytes[i] = (i * 37 + 13) & 0xff;
    const encoded = b64encode(bytes);
    const decoded = b64decode(encoded);
    assert.deepEqual(decoded, bytes, `mismatch at buffer length ${len}`);
  }
});

test('base64: rejects invalid lengths (length mod 4 == 1)', () => {
  assert.equal(b64decode('A'), null);
  assert.equal(b64decode('AAAAA'), null);
  assert.equal(b64decode('AAAA='), null); // 4 non-pad + 1 pad
});

test('base64: rejects non-alphabet characters and URL-safe substitutes', () => {
  assert.equal(b64decode('AAAA!'), null);
  assert.equal(b64decode('AA-A'), null); // standard RFC 4648 rejects '-'
  assert.equal(b64decode('AA_A'), null); // standard RFC 4648 rejects '_'
  assert.equal(b64decode('AAAA\n'), null);
  assert.equal(b64decode(' AAAA'), null);
});

test('base64: rejects non-zero trailing bits in final sextet', () => {
  // 'Zm9=' has 2 unused bits that must be 0; non-zero must fail
  assert.equal(b64decode('Zm9='), null);
  // 'ZE==' has 4 unused bits; 'Zg==' is valid ('f'), 'ZE==' has bad bits
  assert.equal(b64decode('ZE=='), null);
});

test('base64: rejects stray or interior padding', () => {
  assert.equal(b64decode('AA=A'), null);
  assert.equal(b64decode('A==='), null);
});
