/**
 * Unit tests for agent identity and the working indicator. Run with `npm test -w @chernobyl/mobile`.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { PermissionRequest, TranscriptItem, TurnState } from '@chernobyl/protocol';
import {
  VERB_MS,
  WORKING_VERBS,
  formatElapsed,
  markFor,
  workingA11yLabel,
  workingLabel,
  workingStatus,
  workingVerb,
} from './agentIdentity.ts';
import { agentColors, colors } from '../theme/tokens.ts';

test('markFor: each runtime family has its mark, everything else is generic', () => {
  assert.equal(markFor('claude-code'), 'claude');
  assert.equal(markFor('codex'), 'codex');
  assert.equal(markFor('opencode'), 'opencode');
  for (const k of ['local', 'browser', 'custom', 'ollama', 'something-new', undefined]) assert.equal(markFor(k), 'generic');
});

test('workingVerb: steady within a step, changes every VERB_MS, seeded per turn', () => {
  assert.ok(WORKING_VERBS.length >= 20);
  assert.equal(new Set(WORKING_VERBS).size, WORKING_VERBS.length);
  const a = workingVerb(0, 't1');
  assert.equal(workingVerb(VERB_MS - 1, 't1'), a);
  assert.notEqual(workingVerb(VERB_MS, 't1'), a);
  assert.equal(workingVerb(-5, 't1'), a);
  // the whole list comes round
  const seen = new Set(WORKING_VERBS.map((_, i) => workingVerb(i * VERB_MS, 't1')));
  assert.equal(seen.size, WORKING_VERBS.length);
  // different turns start at different places (for these seeds)
  assert.notEqual(workingVerb(0, 'turn-a'), workingVerb(0, 'turn-b'));
});

const turn = (id: string, status: TurnState['status'], startedAt = 1000): TurnState => ({ id, status, startedAt });
const tool = (id: string, turnId: string, status: 'pending' | 'running' | 'awaiting-approval' | 'completed', name = 'Bash'): TranscriptItem => ({
  id,
  ts: 0,
  turnId,
  kind: 'tool',
  tool: { name, status },
});
const text = (id: string, turnId: string): TranscriptItem => ({ id, ts: 0, turnId, kind: 'text', text: 'hi', done: false });
const pending = { id: 'p1', status: 'pending' } as PermissionRequest;

test('workingStatus: hidden without a running turn', () => {
  assert.equal(workingStatus([], [], []), null);
  assert.equal(workingStatus([text('a', 't1')], [turn('t1', 'completed'), turn('t2', 'interrupted')], []), null);
});

test('workingStatus: thinking, running a tool, waiting', () => {
  const turns = [turn('t0', 'completed', 0), turn('t1', 'running')];
  assert.deepEqual(workingStatus([text('a', 't1')], turns, []), { state: 'thinking', turn: turns[1] });
  assert.deepEqual(workingStatus([text('a', 't1'), tool('b', 't1', 'running', 'Edit')], turns, []), {
    state: 'working',
    turn: turns[1],
    tool: 'Edit',
  });
  assert.equal(workingStatus([tool('b', 't1', 'pending')], turns, [])?.state, 'working');
  // a finished tool: the model is back to thinking
  assert.equal(workingStatus([tool('b', 't1', 'completed')], turns, [])?.state, 'thinking');
  assert.equal(workingStatus([tool('b', 't1', 'awaiting-approval')], turns, [])?.state, 'waiting');
  assert.equal(workingStatus([text('a', 't1')], turns, [pending])?.state, 'waiting');
  // only the running turn's items count
  assert.equal(workingStatus([tool('b', 't1', 'completed'), tool('c', 't0', 'running')], turns, [])?.state, 'thinking');
});

test('workingStatus: the newest running turn wins', () => {
  const turns = [turn('new', 'running', 2000), turn('old', 'running', 1000)];
  assert.equal(workingStatus([], turns, [])?.turn.id, 'new');
});

test('workingLabel and the screen-reader label', () => {
  assert.equal(workingLabel('thinking', 0, 't1'), `${workingVerb(0, 't1')}…`);
  assert.equal(workingLabel('working', 0, 't1', 'Bash'), 'Running Bash…');
  assert.equal(workingLabel('working', 0, 't1'), 'Running a tool…');
  assert.equal(workingLabel('waiting', 0, 't1'), 'Waiting for you');
  // one label for thinking and tools, so verbs, ticks and tool calls never re-announce
  assert.equal(workingA11yLabel('thinking', 'Claude Code'), 'Claude Code is working');
  assert.equal(workingA11yLabel('working', 'Claude Code'), 'Claude Code is working');
  assert.equal(workingA11yLabel('waiting', 'Codex'), 'Codex is waiting for you');
});

test('formatElapsed', () => {
  assert.equal(formatElapsed(-1), '0s');
  assert.equal(formatElapsed(8_900), '8s');
  assert.equal(formatElapsed(65_000), '1m 05s');
  assert.equal(formatElapsed(3_725_000), '1h 02m');
});

/** WCAG relative luminance contrast */
function contrast(a: string, b: string): number {
  const lum = (hex: string) => {
    const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
    const [r, g, bl] = c.map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

test('agent accents keep 3:1 non-text contrast on white cards and the black header', () => {
  for (const [name, hex] of Object.entries(agentColors)) {
    assert.ok(contrast(hex, colors.white) >= 3, `${name} on white: ${contrast(hex, colors.white).toFixed(2)}`);
    assert.ok(contrast(hex, colors.black) >= 3, `${name} on black: ${contrast(hex, colors.black).toFixed(2)}`);
  }
});
