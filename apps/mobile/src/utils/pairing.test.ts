import { test } from 'node:test';
import assert from 'node:assert/strict';
import { pairingNotices } from './pairing.ts';

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
