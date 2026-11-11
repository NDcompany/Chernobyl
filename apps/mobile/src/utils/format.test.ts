/**
 * Unit tests for display formatting helpers. Run with `node --test src/utils/format.test.ts` from apps/mobile.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  agentKindLabel,
  agentStatusLabel,
  agentTone,
  countdown,
  hostStatusLabel,
  hostTone,
  relativeTime,
  sessionStatusLabel,
  transportStatusLabel,
  transportTone,
} from './format.ts';

test('relativeTime: sub-minute boundaries and future timestamps', () => {
  const now = 1_000_000;
  // future timestamp clamped to 0 -> 'just now'
  assert.equal(relativeTime(now + 5000, now), 'just now');
  assert.equal(relativeTime(now, now), 'just now');
  assert.equal(relativeTime(now - 44_999, now), 'just now');
  assert.equal(relativeTime(now - 45_000, now), '1m ago');
  assert.equal(relativeTime(now - 89_999, now), '1m ago');
  assert.equal(relativeTime(now - 90_000, now), '1m ago');
  assert.equal(relativeTime(now - 120_000, now), '2m ago');
});

test('relativeTime: minutes, hours and day boundaries', () => {
  const now = 10_000_000;
  assert.equal(relativeTime(now - 59 * 60_000, now), '59m ago');
  assert.equal(relativeTime(now - 60 * 60_000, now), '1h ago');
  assert.equal(relativeTime(now - 23 * 3600_000, now), '23h ago');
  assert.equal(relativeTime(now - 24 * 3600_000, now), 'yesterday');
  assert.equal(relativeTime(now - 47 * 3600_000, now), 'yesterday');
  assert.equal(relativeTime(now - 48 * 3600_000, now), '2d ago');
  assert.equal(relativeTime(now - 7 * 86400_000, now), '7d ago');
});

test('countdown: expired and exact zero', () => {
  const now = 50_000;
  assert.equal(countdown(50_000, now), 'expired');
  assert.equal(countdown(49_999, now), 'expired');
  assert.equal(countdown(0, now), 'expired');
});

test('countdown: sub-minute countdowns format seconds', () => {
  const now = 10_000;
  assert.equal(countdown(now + 1, now), 'expires in 1s');
  assert.equal(countdown(now + 999, now), 'expires in 1s');
  assert.equal(countdown(now + 1_000, now), 'expires in 1s');
  assert.equal(countdown(now + 1_001, now), 'expires in 2s');
  assert.equal(countdown(now + 59_000, now), 'expires in 59s');
});

test('countdown: minute countdowns pad seconds', () => {
  const now = 10_000;
  assert.equal(countdown(now + 60_000, now), 'expires in 1:00');
  assert.equal(countdown(now + 65_000, now), 'expires in 1:05');
  assert.equal(countdown(now + 125_000, now), 'expires in 2:05');
  assert.equal(countdown(now + 599_000, now), 'expires in 9:59');
});

test('hostTone and hostStatusLabel: all states and fallbacks', () => {
  assert.equal(hostTone('connected'), 'success');
  assert.equal(hostTone('connecting'), 'warning');
  assert.equal(hostTone('disconnected'), 'neutral');
  assert.equal(hostTone('error'), 'neutral');

  assert.equal(hostStatusLabel('connected'), 'Connected');
  assert.equal(hostStatusLabel('connecting'), 'Connecting…');
  assert.equal(hostStatusLabel('disconnected'), 'Offline');
  assert.equal(hostStatusLabel('error'), 'Offline');
});

test('transportTone and transportStatusLabel: all states and fallbacks', () => {
  assert.equal(transportTone('connected'), 'success');
  assert.equal(transportTone('connecting'), 'warning');
  assert.equal(transportTone('handshaking'), 'warning');
  assert.equal(transportTone('reconnecting'), 'warning');
  assert.equal(transportTone('denied'), 'error');
  assert.equal(transportTone('disconnected'), 'neutral');

  assert.equal(transportStatusLabel('connected'), 'Connected');
  assert.equal(transportStatusLabel('connecting'), 'Connecting…');
  assert.equal(transportStatusLabel('handshaking'), 'Authorizing…');
  assert.equal(transportStatusLabel('reconnecting'), 'Reconnecting…');
  assert.equal(transportStatusLabel('denied'), 'Denied');
  assert.equal(transportStatusLabel('disconnected'), 'Offline');
});

test('agentTone and agentStatusLabel: online, busy, idle, offline', () => {
  assert.equal(agentTone('online'), 'success');
  assert.equal(agentTone('busy'), 'warning');
  assert.equal(agentTone('idle'), 'neutral');
  assert.equal(agentTone('offline'), 'neutral');

  assert.equal(agentStatusLabel('online'), 'Online');
  assert.equal(agentStatusLabel('busy'), 'Busy');
  assert.equal(agentStatusLabel('idle'), 'Idle');
  assert.equal(agentStatusLabel('offline'), 'Offline');
});

test('sessionStatusLabel: running, waiting, completed, interrupted, idle', () => {
  assert.equal(sessionStatusLabel('running'), 'Running');
  assert.equal(sessionStatusLabel('waiting'), 'Waiting');
  assert.equal(sessionStatusLabel('completed'), 'Completed');
  assert.equal(sessionStatusLabel('interrupted'), 'Interrupted');
  assert.equal(sessionStatusLabel('idle'), 'Idle');
});

test('agentKindLabel: recognized kinds and custom fallback', () => {
  assert.equal(agentKindLabel('claude-code'), 'Claude Code');
  assert.equal(agentKindLabel('opencode'), 'OpenCode');
  assert.equal(agentKindLabel('codex'), 'Codex');
  assert.equal(agentKindLabel('browser'), 'Browser automation');
  assert.equal(agentKindLabel('local'), 'Local agent');
  assert.equal(agentKindLabel('unknown-custom'), 'Custom agent');
  assert.equal(agentKindLabel(''), 'Custom agent');
});
