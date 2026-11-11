/**
 * Unit tests for logoPath vector assets. Run with `node --test src/ui/logoPath.test.ts` from apps/mobile.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { LOGO_MARK_PATH } from './logoPath.ts';

test('logoPath: non-empty SVG path data string', () => {
  assert.equal(typeof LOGO_MARK_PATH, 'string');
  assert.ok(LOGO_MARK_PATH.length > 500);
});

test('logoPath: valid SVG path commands and bounds in 100x100 viewBox', () => {
  assert.ok(LOGO_MARK_PATH.startsWith('M'));
  assert.ok(LOGO_MARK_PATH.endsWith('Z'));

  // Extract all coordinates and verify they fit in a 0..100 viewBox
  const numbers = LOGO_MARK_PATH.match(/-?\d+(?:\.\d+)?/g)?.map(Number) ?? [];
  assert.ok(numbers.length > 50);
  for (const n of numbers) {
    assert.ok(n >= -0.1 && n <= 100.1, `coordinate ${n} must fit within the 100x100 box`);
  }
});
