/**
 * ADR-016: workspace message validators and the golden `workspace_result`
 * fixtures (the same files services/companion-rs reads with include_str!).
 * The client fixtures for these messages live in client-valid.json /
 * client-invalid.json and are covered by live.test.ts.
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import {
  FEATURE_TERMINAL,
  MockEngine,
  TERMINAL_COMMAND_MAX,
  WORKSPACE_ID_MAX,
  WORKSPACE_MESSAGE_MAX,
  WORKSPACE_PATHS_MAX,
  WORKSPACE_PATH_MAX,
  isClientMessage,
  isServerMessage,
} from './index';

const fixture = (name: string): unknown =>
  JSON.parse(readFileSync(join(__dirname, '..', 'fixtures', 'live', name), 'utf8'));

test('workspace bounds: the maximum passes, one more fails', () => {
  const stage = (paths: unknown) => ({ type: 'git_stage', commandId: 'c', workspaceId: 'w', paths });
  assert.ok(isClientMessage(stage(Array<string>(WORKSPACE_PATHS_MAX).fill('p'.repeat(WORKSPACE_PATH_MAX)))));
  assert.ok(!isClientMessage(stage(Array<string>(WORKSPACE_PATHS_MAX + 1).fill('p'))));
  assert.ok(!isClientMessage(stage(['p'.repeat(WORKSPACE_PATH_MAX + 1)])));
  const commit = (message: string) => ({ type: 'git_commit', commandId: 'c', workspaceId: 'w', message });
  assert.ok(isClientMessage(commit('m'.repeat(WORKSPACE_MESSAGE_MAX))));
  assert.ok(!isClientMessage(commit('m'.repeat(WORKSPACE_MESSAGE_MAX + 1))));
  const status = (workspaceId: string) => ({ type: 'git_status', commandId: 'c', workspaceId });
  assert.ok(isClientMessage(status('w'.repeat(WORKSPACE_ID_MAX))));
  assert.ok(!isClientMessage(status('w'.repeat(WORKSPACE_ID_MAX + 1))));
});

test('workspace_result fixtures validate; malformed ones are dropped', () => {
  const results = fixture('workspace-results.json') as Array<Record<string, unknown>>;
  assert.deepEqual(
    results.map((r) => (r.result as { kind: string }).kind),
    ['fs_list', 'fs_read', 'git_status', 'git_diff'],
  );
  for (const r of results) assert.ok(isServerMessage(r), JSON.stringify(r));
  const [first] = results;
  assert.ok(!isServerMessage({ ...first, result: 'x' }));
  assert.ok(!isServerMessage({ ...first, result: {} }));
  assert.ok(!isServerMessage({ ...first, commandId: '' }));
  assert.ok(!isServerMessage({ ...first, workspaceId: undefined }));
});

test('the simulation refuses workspace actions honestly (ADR-009)', () => {
  const engine = new MockEngine(() => undefined);
  const reply = engine.handle({ type: 'git_status', commandId: 'c1', workspaceId: 'w' });
  assert.equal(reply?.type, 'ack');
  assert.equal(reply?.type === 'ack' && reply.ok, false);
  engine.dispose();
});

test('git_pull command_result: agentConfigChanged is optional and must be a string list', () => {
  const base = {
    type: 'command_result',
    commandId: 'c',
    status: 'completed',
    seq: 1,
    eventId: 'e',
    occurredAt: 1,
  };
  assert.ok(isServerMessage(base), 'old shape stays valid');
  assert.ok(isServerMessage({ ...base, agentConfigChanged: [] }));
  assert.ok(isServerMessage({ ...base, agentConfigChanged: ['.claude/settings.json', 'CLAUDE.md'] }));
  assert.ok(!isServerMessage({ ...base, agentConfigChanged: 'CLAUDE.md' }));
  assert.ok(!isServerMessage({ ...base, agentConfigChanged: [1] }));
});

test('ADR-020 terminal: command bounds, golden server messages, honest simulation', () => {
  assert.equal(FEATURE_TERMINAL, 'terminal.v1');
  const run = (command: unknown) => ({ type: 'terminal_run', commandId: 'c', workspaceId: 'w', command });
  assert.ok(isClientMessage(run('x'.repeat(TERMINAL_COMMAND_MAX))));
  assert.ok(!isClientMessage(run('x'.repeat(TERMINAL_COMMAND_MAX + 1))));
  assert.ok(!isClientMessage(run('')));
  assert.ok(isClientMessage({ type: 'terminal_stop', commandId: 'c', workspaceId: 'w' }));
  assert.ok(isClientMessage({ type: 'git_pull', commandId: 'c', workspaceId: 'w' }));
  assert.ok(!isClientMessage({ type: 'terminal_stop', commandId: 'c', workspaceId: '' }));

  const frames = fixture('terminal.json') as Array<Record<string, unknown>>;
  assert.deepEqual(
    frames.map((f) => f.type),
    ['terminal_output', 'terminal_exit', 'terminal_exit', 'terminal_exit'],
  );
  for (const f of frames) assert.ok(isServerMessage(f), JSON.stringify(f));
  const [output, exit] = frames;
  assert.ok(!isServerMessage({ ...output, data: 3 }));
  assert.ok(!isServerMessage({ ...exit, reason: 'crashed' }));
  assert.ok(!isServerMessage({ ...exit, exitCode: '0' }));
  assert.ok(!isServerMessage({ ...exit, truncated: undefined }));

  const engine = new MockEngine(() => undefined);
  for (const msg of [
    { type: 'terminal_run', commandId: 't1', workspaceId: 'w', command: 'ls' },
    { type: 'git_pull', commandId: 't2', workspaceId: 'w' },
  ] as const) {
    const reply = engine.handle(msg);
    assert.equal(reply?.type === 'ack' && reply.ok, false);
  }
  engine.dispose();
});
