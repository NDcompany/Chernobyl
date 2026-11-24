import { test } from 'node:test';
import assert from 'node:assert/strict';
import { keepTokenOnPair, pairingNotices } from './pairing.ts';

test('a first pairing typed or scanned in the app has no warning', () => {
  assert.deepEqual(pairingNotices(null, false), []);
});

test('an existing pairing says it is replaced', () => {
  assert.deepEqual(pairingNotices('my-pc', false), ['This replaces your current host my-pc']);
});

test('a link warns about its origin, and about the replacement when one exists', () => {
  assert.equal(pairingNotices(null, true).length, 1);
  assert.match(pairingNotices(null, true)[0]!, /only continue if you started this pairing/);
  const both = pairingNotices('my-pc', true);
  assert.equal(both.length, 2);
  assert.match(both[0]!, /my-pc/);
});

test('a token typed before the first pairing is kept for it', () => {
  assert.equal(keepTokenOnPair(null, null, 'pc-a'), true);
});

test('a token entered for this host survives re-pairing it', () => {
  assert.equal(keepTokenOnPair('pc-a', 'pc-a', 'pc-a'), true);
  assert.equal(keepTokenOnPair('pc-a', null, 'pc-a'), true);
});

test("another host's token is never carried to a new host", () => {
  assert.equal(keepTokenOnPair('pc-a', 'pc-a', 'pc-b'), false);
  assert.equal(keepTokenOnPair('pc-a', null, 'pc-b'), false);
  // unbound legacy token: it belongs to the pairing stored when it was saved
  assert.equal(keepTokenOnPair(null, 'pc-a', 'pc-b'), false);
  assert.equal(keepTokenOnPair(null, 'pc-a', 'pc-a'), true);
});
