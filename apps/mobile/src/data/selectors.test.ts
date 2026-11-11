/**
 * Unit tests for snapshot selectors. Run with `node --test src/data/selectors.test.ts` from apps/mobile.
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

let selectorsMod: typeof import('./selectors.ts') | null = null;
try {
  selectorsMod = await import('./selectors.ts');
} catch {
  // @chernobyl/protocol/dist/index.js is built by npm run build:protocol prior to npm test in CI.
  // When running node --test directly without running the workspace compiler first, dynamic import is caught here.
}

const makeHost = (id: string, name = 'Host', simulated = false): Host => ({
  id,
  name,
  status: 'connected',
  platform: 'win32',
  version: '1.0.0',
  address: '127.0.0.1',
  daemonStartedAt: 1000,
  lastHeartbeatAt: 2000,
  capabilities: ['claude-code'],
  simulated,
});

const makeAgent = (id: string, name = 'Agent', hostId = 'h1', simulated = false): Agent => ({
  id,
  name,
  hostId,
  kind: 'claude-code',
  state: 'online',
  capabilities: ['shell', 'file-write'],
  activeSessions: 0,
  simulated,
});

const makeSession = (id: string, agentId = 'a1', status: Session['status'] = 'running', transcript = false): Session => ({
  id,
  agentId,
  hostId: 'h1',
  title: `Session ${id}`,
  project: `project-${id}`,
  status,
  progress: 50,
  createdAt: 1000,
  lastActivityAt: 1000,
  timeline: [],
  output: [],
  files: [],
  transcript,
});

const makePermission = (
  id: string,
  status: PermissionRequest['status'] = 'pending',
  expiresAt = 2000,
  decidedAt?: number,
): PermissionRequest => ({
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
  status,
  supportedDecisions: ['approve-once', 'reject'],
  revision: 1,
  requestedAt: 1000,
  expiresAt,
  context: '',
  decidedAt,
});

test('isPending and pendingPermissions: filters out expired or non-pending requests', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { isPending, pendingPermissions } = selectorsMod!;
  const p1 = makePermission('p1', 'pending', 2000);
  const p2 = makePermission('p2', 'pending', 1500);
  const p3 = makePermission('p3', 'approved', 3000);
  const p4 = makePermission('p4', 'rejected', 3000);

  assert.equal(isPending(p1, 1999), true);
  assert.equal(isPending(p1, 2000), false);
  assert.equal(isPending(p1, 2001), false);
  assert.equal(isPending(p3, 1000), false);

  const snapshot: Snapshot = {
    hosts: [],
    agents: [],
    sessions: [],
    permissions: [p1, p2, p3, p4],
    activity: [],
  };

  const pending = pendingPermissions(snapshot, 1600);
  assert.deepEqual(pending.map((p) => p.id), ['p1']);
});

test('decidedPermissions: approved and rejected sorted descending by decidedAt / requestedAt', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { decidedPermissions } = selectorsMod!;
  const p1 = makePermission('p1', 'approved', 2000, 1500);
  const p2 = makePermission('p2', 'rejected', 2000, 1800);
  const p3 = makePermission('p3', 'pending', 2000);
  const p4 = makePermission('p4', 'approved', 2000, undefined);

  const snapshot: Snapshot = {
    hosts: [],
    agents: [],
    sessions: [],
    permissions: [p1, p2, p3, p4],
    activity: [],
  };

  const decided = decidedPermissions(snapshot);
  assert.deepEqual(decided.map((p) => p.id), ['p2', 'p1', 'p4']);
});

test('hasSimulatedData: detects simulation flag on hosts or agents', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { hasSimulatedData } = selectorsMod!;
  const sEmpty: Snapshot = { hosts: [], agents: [], sessions: [], permissions: [], activity: [] };
  assert.equal(hasSimulatedData(sEmpty), false);

  const sHostSim: Snapshot = { ...sEmpty, hosts: [makeHost('h1', 'H', true)] };
  assert.equal(hasSimulatedData(sHostSim), true);

  const sAgentSim: Snapshot = { ...sEmpty, hosts: [makeHost('h1')], agents: [makeAgent('a1', 'A', 'h1', true)] };
  assert.equal(hasSimulatedData(sAgentSim), true);

  const sReal: Snapshot = { ...sEmpty, hosts: [makeHost('h1')], agents: [makeAgent('a1')] };
  assert.equal(hasSimulatedData(sReal), false);
});

test('lookup by ID: hostById, agentById, sessionById, permissionById', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { hostById, agentById, sessionById, permissionById } = selectorsMod!;
  const h = makeHost('h1');
  const a = makeAgent('a1');
  const s = makeSession('s1');
  const p = makePermission('p1');
  const snapshot: Snapshot = {
    hosts: [h],
    agents: [a],
    sessions: [s],
    permissions: [p],
    activity: [],
  };

  assert.equal(hostById(snapshot, 'h1'), h);
  assert.equal(hostById(snapshot, 'none'), undefined);
  assert.equal(agentById(snapshot, 'a1'), a);
  assert.equal(sessionById(snapshot, 's1'), s);
  assert.equal(permissionById(snapshot, 'p1'), p);
});

test('permissionHref: routes to live transcript session with focus or standard screen', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { permissionHref } = selectorsMod!;
  const sessTranscript = makeSession('s-live', 'a1', 'running', true);
  const sessPlain = makeSession('s-plain', 'a1', 'running', false);
  const snapshot: Snapshot = {
    hosts: [],
    agents: [],
    sessions: [sessTranscript, sessPlain],
    permissions: [],
    activity: [],
  };

  const permWithTool = { ...makePermission('p1'), sessionId: 's-live', toolItemId: 'tool-42' };
  assert.equal(permissionHref(snapshot, permWithTool, true), '/session/s-live?focus=tool-42');

  assert.equal(permissionHref(snapshot, permWithTool, false), '/permission/p1');

  const permNoTool = { ...makePermission('p2'), sessionId: 's-live', toolItemId: undefined };
  assert.equal(permissionHref(snapshot, permNoTool, true), '/permission/p2');

  const permPlain = { ...makePermission('p3'), sessionId: 's-plain', toolItemId: 'tool-42' };
  assert.equal(permissionHref(snapshot, permPlain, true), '/permission/p3');
});

test('sessionsForAgent and activeSessions: filters and sorts', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { sessionsForAgent, activeSessions } = selectorsMod!;
  const s1 = { ...makeSession('s1', 'a1', 'completed'), lastActivityAt: 2000 };
  const s2 = { ...makeSession('s2', 'a1', 'running'), lastActivityAt: 3000 };
  const s3 = { ...makeSession('s3', 'a1', 'waiting'), lastActivityAt: 1000 };
  const s4 = { ...makeSession('s4', 'a2', 'running'), lastActivityAt: 4000 };

  const snapshot: Snapshot = {
    hosts: [],
    agents: [],
    sessions: [s1, s2, s3, s4],
    permissions: [],
    activity: [],
  };

  const a1Sessions = sessionsForAgent(snapshot, 'a1');
  assert.deepEqual(a1Sessions.map((x) => x.id), ['s2', 's1', 's3']);

  const allActive = activeSessions(snapshot);
  assert.deepEqual(allActive.map((x) => x.id), ['s2', 's3', 's4']);

  const a1Active = activeSessions(snapshot, 'a1');
  assert.deepEqual(a1Active.map((x) => x.id), ['s2', 's3']);
});

test('counts: computes connected hosts, online/busy agents, active sessions and pending permissions', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { counts } = selectorsMod!;
  const snapshot: Snapshot = {
    hosts: [
      makeHost('h1'),
      { ...makeHost('h2'), status: 'disconnected' },
    ],
    agents: [
      makeAgent('a1'),
      { ...makeAgent('a2'), state: 'busy' },
      { ...makeAgent('a3'), state: 'idle' },
      { ...makeAgent('a4'), state: 'offline' },
    ],
    sessions: [
      makeSession('s1', 'a1', 'running'),
      makeSession('s2', 'a1', 'waiting'),
      makeSession('s3', 'a1', 'completed'),
    ],
    permissions: [
      makePermission('p1', 'pending', 2000),
      makePermission('p2', 'pending', 500),
    ],
    activity: [],
  };

  const c = counts(snapshot, 1000);
  assert.deepEqual(c, {
    hosts: 1,
    agents: 2,
    running: 2,
    pending: 1,
  });
});

test('recentActivity and activityFor: sorts and slices', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { recentActivity, activityFor } = selectorsMod!;
  const events: ActivityEvent[] = [
    { id: 'e1', type: 'session_started', source: 'agent', actorId: 'a1', refId: 's1', title: 'E1', description: '', level: 'info', ts: 10 },
    { id: 'e2', type: 'session_started', source: 'agent', actorId: 'a1', refId: 's2', title: 'E2', description: '', level: 'info', ts: 20 },
    { id: 'e3', type: 'session_started', source: 'agent', actorId: 'a1', refId: 's1', title: 'E3', description: '', level: 'info', ts: 30 },
  ];
  const snapshot: Snapshot = { hosts: [], agents: [], sessions: [], permissions: [], activity: events };

  const recent = recentActivity(snapshot, 2);
  assert.deepEqual(recent.map((e) => e.id), ['e3', 'e2']);

  const forS1 = activityFor(snapshot, new Set(['s1']));
  assert.deepEqual(forS1.map((e) => e.id), ['e3', 'e1']);
});

test('searchAgents: matches against name, kind, capabilities and host name', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { searchAgents } = selectorsMod!;
  const h1 = makeHost('h1', 'Primary Desktop');
  const h2 = makeHost('h2', 'Remote Server');
  const a1 = { ...makeAgent('a1', 'Haiku Assistant', 'h1'), kind: 'claude-code', capabilities: ['shell'] };
  const a2 = { ...makeAgent('a2', 'Ollama Qwen', 'h2'), kind: 'opencode', capabilities: ['chat', 'vision'] };
  const snapshot: Snapshot = { hosts: [h1, h2], agents: [a1, a2], sessions: [], permissions: [], activity: [] };

  assert.equal(searchAgents(snapshot, '').length, 2);
  assert.deepEqual(searchAgents(snapshot, 'haiku').map((a) => a.id), ['a1']);
  assert.deepEqual(searchAgents(snapshot, 'opencode').map((a) => a.id), ['a2']);
  assert.deepEqual(searchAgents(snapshot, 'vision').map((a) => a.id), ['a2']);
  assert.deepEqual(searchAgents(snapshot, 'primary').map((a) => a.id), ['a1']);
  assert.equal(searchAgents(snapshot, 'nonexistent').length, 0);
});

test('searchSessions: filters by status and matches title, project or agent name', {
  skip: !selectorsMod ? 'Requires compiled @chernobyl/protocol dist (built by npm run typecheck/build in CI)' : false,
}, () => {
  const { searchSessions } = selectorsMod!;
  const a1 = makeAgent('a1', 'Claude');
  const s1 = { ...makeSession('s1', 'a1', 'running'), title: 'Refactor auth', project: 'web-backend' };
  const s2 = { ...makeSession('s2', 'a1', 'completed'), title: 'Fix bug', project: 'mobile-app' };
  const snapshot: Snapshot = { hosts: [], agents: [a1], sessions: [s1, s2], permissions: [], activity: [] };

  assert.deepEqual(searchSessions(snapshot, '', 'all').map((s) => s.id), ['s1', 's2']);
  assert.deepEqual(searchSessions(snapshot, '', 'running').map((s) => s.id), ['s1']);
  assert.deepEqual(searchSessions(snapshot, 'auth', 'all').map((s) => s.id), ['s1']);
  assert.deepEqual(searchSessions(snapshot, 'mobile', 'all').map((s) => s.id), ['s2']);
  assert.deepEqual(searchSessions(snapshot, 'claude', 'completed').map((s) => s.id), ['s2']);
  assert.equal(searchSessions(snapshot, 'auth', 'completed').length, 0);
});
