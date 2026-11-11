/**
 * Unit tests for the Terminal helpers (ADR-018). Run with `npm test -w @chernobyl/mobile`.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { HISTORY_MAX, exitLabel, exitOk, remember } from './terminal.ts';

test('history keeps the last 20 distinct command lines, newest first', () => {
  let h: string[] = [];
  h = remember(h, 'npm test');
  h = remember(h, '  git log -1  ');
  h = remember(h, 'npm test');
  assert.deepEqual(h, ['npm test', 'git log -1']);
  assert.deepEqual(remember(h, '   '), h);
  for (let i = 0; i < 30; i++) h = remember(h, `echo ${i}`);
  assert.equal(h.length, HISTORY_MAX);
  assert.equal(h[0], 'echo 29');
  assert.equal(h[HISTORY_MAX - 1], 'echo 10');
});

test('exit lines name the code, the timeout, a stop and the output cap', () => {
  assert.equal(exitLabel({ exitCode: 0, reason: 'exit', truncated: false }), 'Exit code 0');
  assert.equal(exitLabel({ exitCode: 2, reason: 'exit', truncated: true }), 'Exit code 2 · output cut at 1 MiB');
  assert.equal(exitLabel({ exitCode: null, reason: 'timeout', truncated: false }), 'Timed out after 120 s, stopped');
  assert.equal(exitLabel({ exitCode: null, reason: 'stopped', truncated: false }), 'Stopped');
  assert.equal(exitLabel({ exitCode: null, reason: 'exit', truncated: false }), 'Ended without an exit code');
  assert.ok(exitOk({ exitCode: 0, reason: 'exit' }));
  assert.ok(!exitOk({ exitCode: 1, reason: 'exit' }));
  assert.ok(!exitOk({ exitCode: null, reason: 'stopped' }));
});
