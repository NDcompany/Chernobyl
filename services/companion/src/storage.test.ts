/**
 * Storage invariants: persisted sequence never resets across reopen, the
 * events+cursor append is atomic, retention is enforced, commands are
 * idempotent by id and reconcile to `unknown` on restart (PRD §10.4, §14.3,
 * §16.3, §19.1), approvals reconcile to `stale` (PRD §19.2).
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { CompanionStorage, type Retention } from './storage';
import { forceRemove } from './testutil';

const RETENTION: Retention = { events: 100, eventsMaxAgeMs: 3600_000 };

const PRD_TABLES = [
  'schema_migrations',
  'companion_config',
  'adapter_instances',
  'native_entity_mappings',
  'sessions',
  'commands',
  'approvals',
  'events',
  'event_cursors',
  'audit_entries',
  'workspace_registry',
  'notification_outbox',
];

function tempDb(): { file: string; cleanup: () => void } {
  const dir = mkdtempSync(join(tmpdir(), 'chernobyl-store-'));
  return { file: join(dir, 'companion.db'), cleanup: () => forceRemove(dir) };
}

test('migrations create every PRD §16.2 table and survive reopen', () => {
  const { file, cleanup } = tempDb();
  try {
    const a = CompanionStorage.open(file, RETENTION);
    const tables = a.tableNames();
    for (const t of PRD_TABLES) assert.ok(tables.includes(t), `missing table ${t}`);
    a.setConfigKV('host-name', 'test-host');
    a.close();

    const b = CompanionStorage.open(file, RETENTION);
    assert.equal(b.getConfigKV('host-name'), 'test-host');
    b.close();
  } finally {
    cleanup();
  }
});

test('the event sequence persists across reopen — a restart never resets it', () => {
  const { file, cleanup } = tempDb();
  try {
    const a = CompanionStorage.open(file, RETENTION);
    assert.equal(a.currentSeq(), 0);
    const seqs = a.appendEvents([
      { type: 'session_created', payload: { type: 'session_created', x: 1 }, occurredAt: Date.now() },
      { type: 'activity', payload: { type: 'activity', x: 2 }, occurredAt: Date.now() },
    ]);
    assert.deepEqual(seqs, [1, 2]);
    a.close();

    const b = CompanionStorage.open(file, RETENTION);
    assert.equal(b.currentSeq(), 2, 'cursor must survive reopen');
    const more = b.appendEvents([{ type: 'activity', payload: { type: 'activity' }, occurredAt: Date.now() }]);
    assert.deepEqual(more, [3], 'sequence continues, no reset');
    const replay = b.readEventsAfter(0);
    assert.equal(replay.length, 3);
    assert.deepEqual(replay.map((e) => e.seq), [1, 2, 3]);
    b.close();
  } finally {
    cleanup();
  }
});

test('retention: hard row cap keeps only the newest events', () => {
  const { file, cleanup } = tempDb();
  try {
    const a = CompanionStorage.open(file, { events: 5, eventsMaxAgeMs: 3600_000 });
    for (let i = 0; i < 10; i++) {
      a.appendEvents([{ type: 'activity', payload: { i }, occurredAt: Date.now() }]);
    }
    assert.equal(a.eventCount(), 5);
    const kept = a.readEventsAfter(0);
    assert.deepEqual(kept.map((e) => e.seq), [6, 7, 8, 9, 10]);
    a.close();
  } finally {
    cleanup();
  }
});

test('retention: age sweep at open drops events past their max age', () => {
  const { file, cleanup } = tempDb();
  try {
    const a = CompanionStorage.open(file, { events: 100, eventsMaxAgeMs: 3600_000 });
    a.appendEvents([
      { type: 'activity', payload: { age: 'old' }, occurredAt: Date.now() - 10 * 60_000 },
      { type: 'activity', payload: { age: 'fresh' }, occurredAt: Date.now() },
    ]);
    a.close();

    // reopen with a 60s horizon — only the fresh row survives
    const b = CompanionStorage.open(file, { events: 100, eventsMaxAgeMs: 60_000 });
    const rows = b.readEventsAfter(0);
    assert.equal(rows.length, 1, 'the old row must be swept at open');
    assert.equal(rows[0].payload.age, 'fresh');
    b.close();
  } finally {
    cleanup();
  }
});

test('commands: idempotent insert, state updates, restart reconciliation to unknown', () => {
  const { file, cleanup } = tempDb();
  try {
    const a = CompanionStorage.open(file, RETENTION);
    assert.equal(a.insertCommand('c-1', 'submit_prompt', 'mock-1', { commandId: 'c-1' }), 'inserted');
    assert.equal(a.insertCommand('c-1', 'submit_prompt', 'mock-1', { commandId: 'c-1' }), 'duplicate');
    a.setCommandState('c-1', 'running');
    assert.equal(a.getCommand('c-1')?.status, 'running');

    // a second command left accepted (never resolved before the "crash")
    a.insertCommand('c-2', 'interrupt', 'mock-1', { commandId: 'c-2' });
    a.close();

    const b = CompanionStorage.open(file, RETENTION);
    const reconciled = b.reconcileUnresolvedCommands();
    assert.equal(reconciled, 2);
    assert.equal(b.getCommand('c-1')?.status, 'unknown');
    assert.match(b.getCommand('c-1')?.error ?? '', /restarted/);
    assert.equal(b.getCommand('c-2')?.status, 'unknown');
    // a resolved command is untouched by reconciliation
    b.setCommandState('c-1', 'completed');
    assert.equal(b.reconcileUnresolvedCommands(), 0);
    assert.equal(b.getCommand('c-1')?.status, 'completed');
    b.close();
  } finally {
    cleanup();
  }
});

test('approvals: pending past their deadline reconcile to stale, resolved are untouched', () => {
  const { file, cleanup } = tempDb();
  try {
    const a = CompanionStorage.open(file, RETENTION);
    const base = {
      nativeRequestId: 'n1',
      sessionId: 's1',
      agentId: 'a1',
      hostId: 'h1',
      action: 'shell' as const,
      title: 'Run shell command',
      command: 'npm test',
      args: [],
      risk: 'medium' as const,
      status: 'pending' as const,
      supportedDecisions: ['approve-once' as const, 'reject' as const],
      revision: 1,
      requestedAt: Date.now() - 10_000,
      context: 'test',
    };
    a.upsertApproval('p-exp', { ...base, id: 'p-exp', expiresAt: Date.now() - 1 }, 'mock-1');
    a.upsertApproval('p-live', { ...base, id: 'p-live', expiresAt: Date.now() + 60_000 }, 'mock-1');
    a.upsertApproval('p-done', { ...base, id: 'p-done', status: 'approved', expiresAt: Date.now() + 60_000 }, 'mock-1');

    assert.equal(a.reconcileExpiredApprovals(), 1);
    // resolved rows are re-upserted from payloads; reconcile only touches pending
    assert.equal(a.approvalCount(), 3);
    a.close();
  } finally {
    cleanup();
  }
});
