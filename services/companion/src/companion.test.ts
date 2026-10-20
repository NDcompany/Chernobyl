/**
 * Companion integration (PRD §25 Phase 2 exit criteria): runs without the
 * mobile app, adapter failure does not crash it, commands and event cursors
 * survive restarts, fail-closed on durable-state loss, loopback diagnostics.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createMockAdapter, type AgentAdapter, type AdapterFaultOptions } from '@chernobyl/adapter-sdk';
import { PROTOCOL_VERSION, type ClientMessage } from '@chernobyl/protocol';
import type { AdapterReport, DiscoveredAdapter } from './adapters';
import { defaultConfig, type CompanionConfig } from './config';
import { createCompanion, type Companion } from './companion';
import { startDiagnostics } from './diagnostics';
import { CompanionStorage, type StoredCommand } from './storage';
import { forceRemove } from './testutil';

interface Spy {
  adapter: AgentAdapter;
  sends: ClientMessage['type'][];
}

function countingMock(opts: AdapterFaultOptions = {}): Spy {
  const inner = createMockAdapter(opts);
  const sends: ClientMessage['type'][] = [];
  const adapter: AgentAdapter = {
    ...inner,
    send: async (msg) => {
      sends.push(msg.type);
      return inner.send(msg);
    },
  };
  return { adapter, sends };
}

function crashyAdapter(): AgentAdapter {
  const boom = () => {
    throw new Error('adapter exploded');
  };
  return {
    id: 'crashy',
    kind: 'custom',
    displayName: 'Crashy',
    capabilities: () => [{ id: 'session.list', status: 'available' }],
    health: async () => boom(),
    listAgents: async () => boom(),
    listSessions: async () => boom(),
    send: async () => boom(),
    subscribe: () => () => {},
    dispose: async () => boom(),
  };
}

function discovered(id: string, adapter: AgentAdapter): DiscoveredAdapter {
  return { entry: { id, type: 'mock' }, adapter };
}

function tempConfig(): { config: CompanionConfig; cleanup: () => void } {
  const dir = mkdtempSync(join(tmpdir(), 'chernobyl-companion-'));
  const config = defaultConfig(dir);
  config.probeIntervalMs = 1000;
  return { config, cleanup: () => forceRemove(dir) };
}

function waitFor(
  fn: () => StoredCommand | undefined,
  pred: (c: StoredCommand) => boolean,
  timeoutMs = 3000,
): Promise<StoredCommand> {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const iv = setInterval(() => {
      const c = fn();
      if (c && pred(c)) {
        clearInterval(iv);
        resolve(c);
      } else if (Date.now() - started > timeoutMs) {
        clearInterval(iv);
        reject(new Error('timed out waiting for command state'));
      }
    }, 15);
  });
}

function reportOf(h: { adapters: AdapterReport[] }, id: string): AdapterReport | undefined {
  return h.adapters.find((a) => a.id === id);
}

test('the companion boots standalone with health, measured resources and a live sequence', async (t) => {
  const { config, cleanup } = tempConfig();
  const companion = await createCompanion({
    config,
    adapters: [discovered('mock-1', countingMock().adapter)],
  });
  t.after(() => companion.close());
  t.after(cleanup);

  const h = companion.health();
  assert.equal(h.protocol, PROTOCOL_VERSION);
  assert.equal(h.storage.ok, true);
  assert.equal(h.storage.file, join(config.dataDir, 'companion.db'));
  assert.equal(reportOf(h, 'mock-1')?.status, 'healthy');
  // resource usage is measured (PRD exit criterion)
  assert.ok(h.resources.rssBytes > 0);
  assert.ok(h.resources.heapUsedBytes > 0);
  assert.ok(h.uptimeMs >= 0);
  // boot audit exists without any client ever connecting
  assert.ok(h.storage.audit >= 1);
});

test('G1: a submitted command persists before execution and follows the adapter lifecycle', async (t) => {
  const { config, cleanup } = tempConfig();
  const { adapter, sends } = countingMock();
  const companion = await createCompanion({ config, adapters: [discovered('mock-1', adapter)] });
  t.after(() => companion.close());
  t.after(cleanup);

  const res = await companion.submitCommand({
    type: 'create_session',
    commandId: 'cmd-live',
    agentId: 'a4',
    title: 'Phase 2 run',
    prompt: 'go',
  });
  assert.equal(res.ok, true);
  assert.equal(res.duplicate, false);
  assert.deepEqual(sends, ['create_session'], 'routed exactly once');

  // the record ends in a terminal state via persisted lifecycle events
  const record = await waitFor(
    () => companion.getCommand('cmd-live'),
    (c) => c.status === 'completed',
  );
  assert.equal(record.adapterId, 'mock-1');
  assert.equal(record.type, 'create_session');

  const h = companion.health();
  assert.ok(h.storage.events > 0, 'events were persisted');
  assert.ok(h.storage.seq >= h.storage.events, 'sequence covers persisted events');
  const replay = companion.readEventsAfter(0);
  assert.ok(replay.some((e) => e.type === 'command_result'));
});

test('commands and cursors survive restart — duplicates return stored state, never re-execute', async (t) => {
  const { config, cleanup } = tempConfig();

  // first process: accept + complete a command
  const first = await createCompanion({
    config,
    adapters: [discovered('mock-1', countingMock().adapter)],
  });
  const before = await first.submitCommand({
    type: 'create_session',
    commandId: 'cmd-dup',
    agentId: 'a4',
    title: 'Once',
    prompt: 'go',
  });
  assert.equal(before.ok, true);
  const seqBefore = first.health().storage.seq;
  assert.ok(seqBefore > 0);
  await first.close();

  // second process over the same data dir: new adapter instance, same commandId
  const { adapter, sends } = countingMock();
  const second = await createCompanion({ config, adapters: [discovered('mock-1', adapter)] });
  t.after(() => second.close());
  t.after(cleanup);

  const dup = await second.submitCommand({
    type: 'create_session',
    commandId: 'cmd-dup',
    agentId: 'a4',
    title: 'Once',
    prompt: 'go',
  });
  assert.equal(dup.ok, true);
  assert.equal(dup.duplicate, true);
  assert.equal(dup.record?.status, 'completed', 'stored state returned');
  assert.deepEqual(sends, [], 'a duplicate must NOT re-execute');

  // the sequence continued from the persisted cursor
  const seqAfter = second.health().storage.seq;
  assert.ok(seqAfter >= seqBefore, 'sequence never resets across restart');
});

test('restart reconciles unresolved commands to unknown (never repeats them)', async (t) => {
  const { config, cleanup } = tempConfig();

  const first = await createCompanion({
    config,
    adapters: [discovered('mock-1', countingMock().adapter)],
  });
  // a command left 'accepted' — simulate a crash before any lifecycle event
  const raw = CompanionStorage.open(join(config.dataDir, 'companion.db'), config.retention);
  raw.insertCommand('cmd-lost', 'submit_prompt', 'mock-1', { commandId: 'cmd-lost' });
  raw.close();
  await first.close();

  const second = await createCompanion({
    config,
    adapters: [discovered('mock-1', countingMock().adapter)],
  });
  t.after(() => second.close());
  t.after(cleanup);
  const record = second.getCommand('cmd-lost');
  assert.equal(record?.status, 'unknown');
  assert.match(record?.error ?? '', /restarted before an outcome/);
});

test('G4.3: adapter failure degrades that adapter but never crashes the companion', async (t) => {
  const { config, cleanup } = tempConfig();
  const { adapter, sends } = countingMock();
  const companion = await createCompanion({
    config,
    adapters: [discovered('crashy', crashyAdapter()), discovered('mock-1', adapter)],
  });
  t.after(() => companion.close());
  t.after(cleanup);

  const h = companion.health();
  assert.equal(reportOf(h, 'crashy')?.status, 'degraded', 'a throwing health() degrades, not crashes');
  assert.equal(reportOf(h, 'mock-1')?.status, 'healthy');

  // commands still route to the healthy adapter
  const res = await companion.submitCommand({
    type: 'create_session',
    commandId: 'cmd-iso',
    agentId: 'a4',
    title: 'Isolation',
    prompt: 'go',
  });
  assert.equal(res.ok, true);
  assert.deepEqual(sends, ['create_session']);

  // an explicit request for the dead adapter fails closed
  const dead = await companion.submitCommand(
    { type: 'create_session', commandId: 'cmd-dead', agentId: 'a4', title: 'x', prompt: 'x' },
    { adapterId: 'crashy' },
  );
  assert.equal(dead.ok, false);
  assert.equal(companion.getCommand('cmd-dead'), undefined, 'nothing executed, nothing persisted');

  // recovery: the crashy adapter starts answering health() again
  const recovered: AgentAdapter = {
    ...crashyAdapter(),
    health: async () => ({ ok: true, detail: 'back online' }),
  };
  const companion2 = await createCompanion({
    config,
    adapters: [discovered('recovering', recovered)],
  });
  assert.equal(reportOf(companion2.health(), 'recovering')?.status, 'healthy');
  await companion2.close();
});

test('PRD §19.4: when durable state cannot be written, commands are refused fail-closed', async (t) => {
  const { config, cleanup } = tempConfig();
  const { adapter, sends } = countingMock();
  const storage = CompanionStorage.open(join(config.dataDir, 'companion.db'), config.retention);
  const companion: Companion = await createCompanion({
    config,
    adapters: [discovered('mock-1', adapter)],
    storage,
  });
  t.after(() => companion.close());
  t.after(cleanup);

  // simulate the disk going away (storage latches on its first write error)
  storage.latchWriteFailure(new Error('disk full'));

  const res = await companion.submitCommand({
    type: 'create_session',
    commandId: 'cmd-failclosed',
    agentId: 'a4',
    title: 'x',
    prompt: 'x',
  });
  assert.equal(res.ok, false);
  assert.match(res.error ?? '', /durable state unavailable/);
  assert.deepEqual(sends, [], 'no execution without a durable record');
  assert.equal(companion.health().storage.ok, false);
  assert.equal(companion.health().ok, false);
});

test('diagnostics: loopback HTTP reports health, version and adapter status', async (t) => {
  const { config, cleanup } = tempConfig();
  const companion = await createCompanion({
    config,
    adapters: [discovered('mock-1', countingMock().adapter)],
  });
  t.after(() => companion.close());
  t.after(cleanup);

  const diag = await startDiagnostics(companion, 0);
  t.after(() => diag.close());
  assert.ok(diag.port > 0);

  const health = await fetch(`http://127.0.0.1:${diag.port}/health`);
  assert.equal(health.status, 200);
  const body = (await health.json()) as { ok: boolean; protocol: number };
  assert.equal(body.protocol, PROTOCOL_VERSION);

  const version = (await (await fetch(`http://127.0.0.1:${diag.port}/version`)).json()) as {
    protocol: number;
  };
  assert.equal(version.protocol, PROTOCOL_VERSION);

  const adapters = (await (await fetch(`http://127.0.0.1:${diag.port}/adapters`)).json()) as
    AdapterReport[];
  assert.equal(adapters[0].id, 'mock-1');

  const missing = await fetch(`http://127.0.0.1:${diag.port}/nope`);
  assert.equal(missing.status, 404);
});

test('discovery: an unsupported adapter type is recorded, never silently ignored', async (t) => {
  const { config, cleanup } = tempConfig();
  config.adapters = [
    { id: 'ghost', type: 'does-not-exist' },
    { id: 'off', type: 'mock', enabled: false },
  ];
  const companion = await createCompanion({ config });
  t.after(() => companion.close());
  t.after(cleanup);

  const report = reportOf(companion.health(), 'ghost');
  assert.equal(report?.status, 'discovery-failed');
  assert.match(report?.detail ?? '', /unsupported adapter type/);
  assert.equal(reportOf(companion.health(), 'off'), undefined, 'disabled entries are skipped');

  // nothing is healthy → commands refuse fail-closed
  const res = await companion.submitCommand({
    type: 'create_session',
    commandId: 'cmd-none',
    agentId: 'a4',
    title: 'x',
    prompt: 'x',
  });
  assert.equal(res.ok, false);
  assert.match(res.error ?? '', /no healthy adapter/);
});

test('close() is idempotent and commands are refused once shutting down', async (t) => {
  const { config, cleanup } = tempConfig();
  t.after(cleanup); // body closes the companion before hooks run
  const companion = await createCompanion({
    config,
    adapters: [discovered('mock-1', countingMock().adapter)],
  });
  await companion.close();
  await companion.close(); // idempotent
  const res = await companion.submitCommand({
    type: 'create_session',
    commandId: 'cmd-late',
    agentId: 'a4',
    title: 'x',
    prompt: 'x',
  });
  assert.equal(res.ok, false);
  assert.match(res.error ?? '', /shutting down/);
});
