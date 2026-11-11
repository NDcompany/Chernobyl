/**
 * Unit tests for snapshot message reducer. Run with `node --test src/data/reducer.test.ts` from apps/mobile.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import type {
  ActivityEvent,
  Agent,
  Host,
  PermissionRequest,
  Session,
  Snapshot,
} from '@chernobyl/protocol';
import { applyServerMessage } from './reducer.ts';

const EMPTY_SNAPSHOT: Snapshot = {
  hosts: [],
  agents: [],
  sessions: [],
  permissions: [],
  activity: [],
};

const makeHost = (id: string, name = 'Host'): Host => ({
  id,
  name,
  status: 'connected',
  platform: 'win32',
  version: '1.0.0',
  address: '127.0.0.1',
  daemonStartedAt: 1000,
  lastHeartbeatAt: 2000,
  capabilities: ['claude-code'],
});

const makeAgent = (id: string, name = 'Agent'): Agent => ({
  id,
  name,
  hostId: 'h1',
  kind: 'claude-code',
  state: 'online',
  capabilities: ['shell'],
  activeSessions: 0,
});

const makeSession = (id: string, title = 'Session'): Session => ({
  id,
  agentId: 'a1',
  hostId: 'h1',
  title,
  project: 'proj',
  status: 'running',
  progress: 10,
  createdAt: 1000,
  lastActivityAt: 1000,
  timeline: [],
  output: [],
  files: [],
});

const makePermission = (id: string): PermissionRequest => ({
  id,
  nativeRequestId: `n-${id}`,
  sessionId: 's1',
  agentId: 'a1',
  hostId: 'h1',
  action: 'shell',
  title: 'Execute command',
  command: 'npm test',
  args: [],
  risk: 'low',
  status: 'pending',
  supportedDecisions: ['approve-once', 'reject'],
  revision: 1,
  requestedAt: 1000,
  expiresAt: 2000,
  context: '',
});

const makeActivity = (id: string, ts = 1000): ActivityEvent => ({
  id,
  type: 'session_started',
  source: 'agent',
  actorId: 'a1',
  refId: 's1',
  title: 'Started session',
  description: 'Running task',
  level: 'info',
  ts,
});

test('applyServerMessage: hello_ack and state replace the entire snapshot', () => {
  const customSnapshot: Snapshot = {
    hosts: [makeHost('h1')],
    agents: [makeAgent('a1')],
    sessions: [makeSession('s1')],
    permissions: [makePermission('p1')],
    activity: [makeActivity('act1')],
  };

  const fromHello = applyServerMessage(EMPTY_SNAPSHOT, {
    type: 'hello_ack',
    protocol: 2,
    serverTime: 1234,
    snapshot: customSnapshot,
    lastSeq: 42,
  });
  assert.deepEqual(fromHello, customSnapshot);

  const fromState = applyServerMessage(EMPTY_SNAPSHOT, {
    type: 'state',
    snapshot: customSnapshot,
    seq: 43,
  });
  assert.deepEqual(fromState, customSnapshot);
});

test('applyServerMessage: host_updated upserts cleanly (prepend new, replace existing in-place)', () => {
  const h1 = makeHost('h1', 'Initial');
  const h2 = makeHost('h2', 'Second');
  const s0 = EMPTY_SNAPSHOT;

  const s1 = applyServerMessage(s0, { type: 'host_updated', host: h1 });
  assert.deepEqual(s1.hosts, [h1]);

  const s2 = applyServerMessage(s1, { type: 'host_updated', host: h2 });
  assert.deepEqual(s2.hosts, [h2, h1]);

  const h1Updated = { ...h1, name: 'Updated' };
  const s3 = applyServerMessage(s2, { type: 'host_updated', host: h1Updated });
  assert.deepEqual(s3.hosts, [h2, h1Updated]);
});

test('applyServerMessage: agent_updated upserts agents', () => {
  const a1 = makeAgent('a1', 'Agent 1');
  const a2 = makeAgent('a2', 'Agent 2');
  const s1 = applyServerMessage(EMPTY_SNAPSHOT, { type: 'agent_updated', agent: a1 });
  assert.deepEqual(s1.agents, [a1]);

  const s2 = applyServerMessage(s1, { type: 'agent_updated', agent: a2 });
  assert.deepEqual(s2.agents, [a2, a1]);

  const a1Busy = { ...a1, state: 'busy' as const };
  const s3 = applyServerMessage(s2, { type: 'agent_updated', agent: a1Busy });
  assert.deepEqual(s3.agents, [a2, a1Busy]);
});

test('applyServerMessage: session_created and session_updated upsert sessions', () => {
  const sess1 = makeSession('s1', 'Session 1');
  const sess2 = makeSession('s2', 'Session 2');
  const s1 = applyServerMessage(EMPTY_SNAPSHOT, { type: 'session_created', session: sess1 });
  assert.deepEqual(s1.sessions, [sess1]);

  const s2 = applyServerMessage(s1, { type: 'session_updated', session: sess2 });
  assert.deepEqual(s2.sessions, [sess2, sess1]);

  const sess1Done = { ...sess1, status: 'completed' as const, progress: 100 };
  const s3 = applyServerMessage(s2, { type: 'session_updated', session: sess1Done });
  assert.deepEqual(s3.sessions, [sess2, sess1Done]);
});

test('applyServerMessage: session_output appends lines, updates progress/status and caps lines at 300', () => {
  const initialLines = Array.from({ length: 290 }, (_, i) => ({
    id: `l-${i}`,
    stream: 'stdout' as const,
    text: `Line ${i}`,
    timestamp: 1000 + i,
  }));
  const baseSession = { ...makeSession('s1'), output: initialLines };
  const state: Snapshot = { ...EMPTY_SNAPSHOT, sessions: [baseSession, makeSession('s2')] };

  const newLines = Array.from({ length: 20 }, (_, i) => ({
    id: `new-${i}`,
    stream: 'stdout' as const,
    text: `New ${i}`,
    timestamp: 2000 + i,
  }));

  const nextState = applyServerMessage(state, {
    type: 'session_output',
    sessionId: 's1',
    lines: newLines,
    progress: 85,
    status: 'running',
  });

  const updatedS1 = nextState.sessions.find((s) => s.id === 's1')!;
  assert.equal(updatedS1.output.length, 300);
  assert.equal(updatedS1.output[0].text, 'Line 10');
  assert.equal(updatedS1.output[299].text, 'New 19');
  assert.equal(updatedS1.progress, 85);
  assert.equal(updatedS1.status, 'running');

  // other session is unmodified
  assert.deepEqual(nextState.sessions.find((s) => s.id === 's2'), makeSession('s2'));
});

test('applyServerMessage: permission_requested and permission_resolved upsert requests', () => {
  const p1 = makePermission('p1');
  const s1 = applyServerMessage(EMPTY_SNAPSHOT, { type: 'permission_requested', permission: p1 });
  assert.deepEqual(s1.permissions, [p1]);

  const p1Resolved = { ...p1, status: 'approved' as const, decision: 'approve-once' as const, decidedAt: 1500 };
  const s2 = applyServerMessage(s1, { type: 'permission_resolved', permission: p1Resolved });
  assert.deepEqual(s2.permissions, [p1Resolved]);
});

test('applyServerMessage: activity prepends and caps at 80 items', () => {
  let state = EMPTY_SNAPSHOT;
  for (let i = 0; i < 90; i++) {
    state = applyServerMessage(state, {
      type: 'activity',
      event: makeActivity(`act-${i}`, i),
    });
  }
  assert.equal(state.activity.length, 80);
  assert.equal(state.activity[0].id, 'act-89');
  assert.equal(state.activity[79].id, 'act-10');
});

test('applyServerMessage: ignores non-state messages such as ack and pong', () => {
  const s0: Snapshot = { ...EMPTY_SNAPSHOT, hosts: [makeHost('h1')] };
  const s1 = applyServerMessage(s0, { type: 'pong', pingId: 'p1' } as any);
  assert.equal(s1, s0);
});
