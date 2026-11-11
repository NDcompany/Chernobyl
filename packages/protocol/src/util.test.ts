import assert from 'node:assert/strict';
import test from 'node:test';
import {
  FEATURE_TRANSCRIPT,
  HEARTBEAT_MISS_LIMIT,
  HEARTBEAT_MS,
  PERMISSION_TTL_MS,
  PROTOCOL_VERSION,
  isStalePermission,
  nextId,
  unrefTimer,
} from './util';

test('protocol constants: frozen protocol version and heartbeat bounds', () => {
  assert.equal(PROTOCOL_VERSION, 2);
  assert.equal(FEATURE_TRANSCRIPT, 'transcript.v1');
  assert.equal(PERMISSION_TTL_MS, 120_000);
  assert.equal(HEARTBEAT_MS, 15_000);
  assert.equal(HEARTBEAT_MISS_LIMIT, 3);
});

test('nextId: produces distinct ids with requested prefix and monotonic counter', () => {
  const id1 = nextId('cmd');
  const id2 = nextId('cmd');
  const id3 = nextId('sub');

  assert.ok(id1.startsWith('cmd_'));
  assert.ok(id2.startsWith('cmd_'));
  assert.ok(id3.startsWith('sub_'));
  assert.notEqual(id1, id2);
  assert.notEqual(id1, id3);

  // Generate a burst of 1000 IDs and verify uniqueness
  const set = new Set<string>();
  for (let i = 0; i < 1000; i++) {
    set.add(nextId('test'));
  }
  assert.equal(set.size, 1000);
});

test('isStalePermission: pending request expiry boundary check', () => {
  const req = { status: 'pending', expiresAt: 5000 };

  // Before deadline
  assert.equal(isStalePermission(req, 4999), false);
  // Exactly at deadline
  assert.equal(isStalePermission(req, 5000), true);
  // After deadline
  assert.equal(isStalePermission(req, 5001), true);
});

test('isStalePermission: non-pending requests are never stale regardless of time', () => {
  const approved = { status: 'approved', expiresAt: 1000 };
  const rejected = { status: 'rejected', expiresAt: 1000 };
  const expired = { status: 'expired', expiresAt: 1000 };

  assert.equal(isStalePermission(approved, 9999), false);
  assert.equal(isStalePermission(rejected, 9999), false);
  assert.equal(isStalePermission(expired, 9999), false);
});

test('unrefTimer: handles timers with unref, without unref, null and undefined', () => {
  let unrefCalled = false;
  const mockTimer = {
    unref: () => {
      unrefCalled = true;
    },
  };
  unrefTimer(mockTimer);
  assert.equal(unrefCalled, true);

  // No unref method (e.g. browser / RN timer ID number)
  assert.doesNotThrow(() => unrefTimer(123));
  assert.doesNotThrow(() => unrefTimer({}));
  assert.doesNotThrow(() => unrefTimer(null));
  assert.doesNotThrow(() => unrefTimer(undefined));
});
