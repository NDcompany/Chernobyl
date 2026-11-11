import assert from 'node:assert/strict';
import test from 'node:test';
import { utf8Decode, utf8Encode } from './utf8.js';

test('utf8: empty and ASCII round-trip', () => {
  assert.equal(utf8Decode(new Uint8Array([])), '');
  assert.deepEqual(utf8Encode(''), new Uint8Array([]));

  const ascii = 'Hello, world! 12345 ~!@#$%^&*()_+';
  assert.equal(utf8Decode(utf8Encode(ascii)), ascii);
});

test('utf8: 2-byte, 3-byte and 4-byte sequences round-trip', () => {
  // 2-byte characters: é, ñ, λ
  const twoByte = 'Café, niño, λλλ';
  assert.equal(utf8Decode(utf8Encode(twoByte)), twoByte);

  // 3-byte characters: €, 中文, 日本語
  const threeByte = 'Price: 100€, 简体中文, ひらがな';
  assert.equal(utf8Decode(utf8Encode(threeByte)), threeByte);

  // 4-byte astral characters: emojis, math symbols
  const fourByte = 'CHERNOBYL 🔥🚀📱 𝕏 \u{1F600}';
  assert.equal(utf8Decode(utf8Encode(fourByte)), fourByte);
});

test('utf8: rejects invalid lead bytes', () => {
  // Continuation byte as lead (0x80..0xBF)
  assert.equal(utf8Decode(new Uint8Array([0x80])), null);
  assert.equal(utf8Decode(new Uint8Array([0xbf])), null);

  // Overlong 2-byte lead (0xC0, 0xC1)
  assert.equal(utf8Decode(new Uint8Array([0xc0, 0xaf])), null);
  assert.equal(utf8Decode(new Uint8Array([0xc1, 0xbf])), null);

  // Out of range leads (0xF5..0xFF)
  assert.equal(utf8Decode(new Uint8Array([0xf5, 0x80, 0x80, 0x80])), null);
  assert.equal(utf8Decode(new Uint8Array([0xff])), null);
});

test('utf8: rejects invalid continuation bytes', () => {
  // 2-byte sequence where 2nd byte is not a continuation byte
  assert.equal(utf8Decode(new Uint8Array([0xc2, 0x20])), null);
  assert.equal(utf8Decode(new Uint8Array([0xc2, 0xc0])), null);

  // 3-byte sequence where 3rd byte is invalid
  assert.equal(utf8Decode(new Uint8Array([0xe2, 0x82, 0x00])), null);

  // 4-byte sequence where 4th byte is invalid
  assert.equal(utf8Decode(new Uint8Array([0xf0, 0x9f, 0x92, 0x01])), null);
});

test('utf8: rejects truncated multi-byte sequences', () => {
  // 2-byte lead alone
  assert.equal(utf8Decode(new Uint8Array([0xc2])), null);
  // 3-byte lead alone or with only 1 continuation byte
  assert.equal(utf8Decode(new Uint8Array([0xe2])), null);
  assert.equal(utf8Decode(new Uint8Array([0xe2, 0x82])), null);
  // 4-byte lead with fewer than 3 continuation bytes
  assert.equal(utf8Decode(new Uint8Array([0xf0])), null);
  assert.equal(utf8Decode(new Uint8Array([0xf0, 0x9f])), null);
  assert.equal(utf8Decode(new Uint8Array([0xf0, 0x9f, 0x92])), null);
});

test('utf8: rejects surrogate code points (0xD800..0xDFFF)', () => {
  // 0xD800: 0xED 0xA0 0x80
  assert.equal(utf8Decode(new Uint8Array([0xed, 0xa0, 0x80])), null);
  // 0xDFFF: 0xED 0xBF 0xBF
  assert.equal(utf8Decode(new Uint8Array([0xed, 0xbf, 0xbf])), null);
});

test('utf8: rejects overlong encodings', () => {
  // Overlong NUL (0xC0 0x80)
  assert.equal(utf8Decode(new Uint8Array([0xc0, 0x80])), null);
  // Overlong 3-byte encoding of codepoints < 0x800 (e.g. 0xE0 0x80 0xAF)
  assert.equal(utf8Decode(new Uint8Array([0xe0, 0x80, 0xaf])), null);
  // Overlong 4-byte encoding of codepoints < 0x10000 (e.g. 0xF0 0x80 0x80 0xAF)
  assert.equal(utf8Decode(new Uint8Array([0xf0, 0x80, 0x80, 0xaf])), null);
});

test('utf8: rejects code points beyond 0x10FFFF', () => {
  // 0x110000 in UTF-8: 0xF4 0x90 0x80 0x80
  assert.equal(utf8Decode(new Uint8Array([0xf4, 0x90, 0x80, 0x80])), null);
});
