/**
 * Canonical mock snapshot used by the host service and by the app's offline
 * (no-host) transport. Single source so the two mocks never drift.
 * Timestamps are relative to the moment `createSeedSnapshot()` is called.
 */

import type {
  ActivityEvent,
  Agent,
  Host,
  PermissionRequest,
  Session,
  Snapshot,
} from './index';

const MIN = 60_000;

export function createSeedSnapshot(now: number = Date.now()): Snapshot {
  const hosts: Host[] = [
    {
      id: 'h1',
      name: 'Workstation-01',
      kind: 'local',
      platform: 'Windows 11',
      address: '192.168.1.14',
      status: 'connected',
      lastSeenAt: now,
    },
    {
      id: 'h2',
      name: 'edge-prod-02',
      kind: 'remote',
      platform: 'Ubuntu 24.04',
      address: '10.0.4.21',
      status: 'connected',
      lastSeenAt: now - 4_000,
    },
    {
      id: 'h3',
      name: 'lab-nas',
      kind: 'remote',
      platform: 'Debian 12',
      address: '192.168.1.90',
      status: 'offline',
      lastSeenAt: now - 42 * MIN,
    },
  ];

  const agents: Agent[] = [
    {
      id: 'a1',
      hostId: 'h1',
      kind: 'claude-code',
      name: 'Claude Code',
      state: 'online',
      capabilities: ['Read & edit files', 'Shell commands', 'Search codebase', 'Plan mode'],
      acceptsNewSession: true,
    },
    {
      id: 'a2',
      hostId: 'h1',
      kind: 'opencode',
      name: 'OpenCode',
      state: 'online',
      capabilities: ['Multi-file edits', 'Terminal tools', 'MCP tools'],
      acceptsNewSession: true,
    },
    {
      id: 'a3',
      hostId: 'h2',
      kind: 'codex',
      name: 'Codex',
      state: 'busy',
      capabilities: ['Code review', 'Test generation', 'Patch application'],
      acceptsNewSession: false,
    },
    {
      id: 'a4',
      hostId: 'h2',
      kind: 'browser',
      name: 'Browser Agent',
      state: 'idle',
      capabilities: ['Navigation', 'Form filling', 'Screenshots'],
      acceptsNewSession: true,
    },
    {
      id: 'a5',
      hostId: 'h3',
      kind: 'local',
      name: 'Local Agent',
      state: 'offline',
      capabilities: ['Offline chat', 'File summary'],
      acceptsNewSession: false,
    },
  ];

  const sessions: Session[] = [
    {
      id: 's1',
      agentId: 'a1',
      hostId: 'h1',
      title: 'Refactor billing module',
      project: 'C:\\Projects\\admin-dashboard',
      status: 'running',
      progress: 64,
      createdAt: now - 9 * MIN,
      lastActivityAt: now - 20_000,
      prompt: 'Split the billing module into invoice, payment and tax services.',
      timeline: [
        { id: 't1', label: 'Read billing module', status: 'done' },
        { id: 't2', label: 'Extract invoice service', status: 'done' },
        { id: 't3', label: 'Extract payment service', status: 'active' },
        { id: 't4', label: 'Update tests', status: 'pending' },
      ],
      output: [
        { id: 'o1', ts: now - 8 * MIN, stream: 'stdout', text: 'Read src/billing/index.ts (412 lines)' },
        { id: 'o2', ts: now - 7 * MIN, stream: 'stdout', text: 'Extracting InvoiceService -> src/billing/invoice.ts' },
        { id: 'o3', ts: now - 5 * MIN, stream: 'stdout', text: 'Extracting PaymentService -> src/billing/payment.ts' },
        { id: 'o4', ts: now - 40_000, stream: 'stdout', text: 'Updating 6 call sites in src/api/routes.ts' },
        { id: 'o5', ts: now - 20_000, stream: 'stdout', text: 'Running type check... 0 errors' },
      ],
      files: [
        { path: 'src/billing/invoice.ts', additions: 148, deletions: 0, status: 'added' },
        { path: 'src/billing/payment.ts', additions: 96, deletions: 0, status: 'added' },
        { path: 'src/billing/index.ts', additions: 12, deletions: 187, status: 'modified' },
        { path: 'src/api/routes.ts', additions: 24, deletions: 24, status: 'modified' },
      ],
    },
    {
      id: 's2',
      agentId: 'a2',
      hostId: 'h1',
      title: 'Install dependencies',
      project: 'C:\\Projects\\admin-dashboard',
      status: 'waiting',
      progress: 30,
      createdAt: now - 3 * MIN,
      lastActivityAt: now - 60_000,
      prompt: 'Install the new dependencies and update the lockfile.',
      timeline: [
        { id: 't1', label: 'Read package.json', status: 'done' },
        { id: 't2', label: 'Run npm install', status: 'active' },
        { id: 't3', label: 'Verify lockfile', status: 'pending' },
      ],
      output: [
        { id: 'o1', ts: now - 3 * MIN, stream: 'stdout', text: 'Detected package-lock.json, npm workspace root found' },
        { id: 'o2', ts: now - 65_000, stream: 'system', text: 'Permission required: run shell command' },
      ],
      files: [],
    },
    {
      id: 's3',
      agentId: 'a3',
      hostId: 'h2',
      title: 'Regression test suite',
      project: 'srv/api-gateway',
      status: 'running',
      progress: 45,
      createdAt: now - 12 * MIN,
      lastActivityAt: now - 8_000,
      prompt: 'Run the regression suite and fix flaky tests.',
      timeline: [
        { id: 't1', label: 'Collect failing tests', status: 'done' },
        { id: 't2', label: 'Run suite', status: 'active' },
        { id: 't3', label: 'Patch flaky tests', status: 'pending' },
      ],
      output: [
        { id: 'o1', ts: now - 11 * MIN, stream: 'stdout', text: 'vitest run --reporter=dot' },
        { id: 'o2', ts: now - 6 * MIN, stream: 'stdout', text: '✓ auth/session.test.ts (18 tests) 241ms' },
        { id: 'o3', ts: now - 3 * MIN, stream: 'stderr', text: '✗ retry/rate-limit.test.ts — timed out after 5000ms' },
        { id: 'o4', ts: now - 8_000, stream: 'stdout', text: 'Re-running 1 flaky test with seeded clock' },
      ],
      files: [{ path: 'tests/retry/rate-limit.test.ts', additions: 8, deletions: 3, status: 'modified' }],
    },
    {
      id: 's4',
      agentId: 'a4',
      hostId: 'h2',
      title: 'Scrape competitor pricing',
      project: 'work/monitoring',
      status: 'completed',
      progress: 100,
      createdAt: now - 60 * MIN,
      lastActivityAt: now - 34 * MIN,
      prompt: 'Collect public pricing pages and summarise plan changes.',
      timeline: [
        { id: 't1', label: 'Open pricing pages', status: 'done' },
        { id: 't2', label: 'Extract plans', status: 'done' },
        { id: 't3', label: 'Write summary', status: 'done' },
      ],
      output: [
        { id: 'o1', ts: now - 40 * MIN, stream: 'stdout', text: 'Visited 4 pricing pages' },
        { id: 'o2', ts: now - 34 * MIN, stream: 'stdout', text: 'Summary written to work/monitoring/pricing-2026-10.md' },
      ],
      files: [{ path: 'work/monitoring/pricing-2026-10.md', additions: 62, deletions: 0, status: 'added' }],
    },
    {
      id: 's5',
      agentId: 'a5',
      hostId: 'h3',
      title: 'Summarize incident logs',
      project: 'logs/2026-10-14',
      status: 'interrupted',
      progress: 20,
      createdAt: now - 3 * 60 * MIN,
      lastActivityAt: now - 2 * 60 * MIN,
      prompt: 'Summarise the incident logs from 14 October.',
      timeline: [
        { id: 't1', label: 'Index log files', status: 'done' },
        { id: 't2', label: 'Summarise', status: 'pending' },
      ],
      output: [
        { id: 'o1', ts: now - 2 * 60 * MIN, stream: 'system', text: 'Interrupted by user after 12m' },
      ],
      files: [],
    },
  ];

  const permissions: PermissionRequest[] = [
    {
      id: 'p1',
      sessionId: 's2',
      agentId: 'a2',
      hostId: 'h1',
      action: 'shell',
      title: 'Run shell command',
      command: 'npm install',
      args: ['--save-dev', 'vitest@^3'],
      directory: 'C:\\Projects\\admin-dashboard',
      risk: 'medium',
      status: 'pending',
      requestedAt: now - 60_000,
      expiresAt: now + 4 * MIN,
      context: 'Adding the test runner requested in the last prompt. Writes node_modules and package-lock.json.',
    },
    {
      id: 'p2',
      sessionId: 's1',
      agentId: 'a1',
      hostId: 'h1',
      action: 'file-write',
      title: 'Write file',
      command: 'src/billing/invoice.ts',
      args: [],
      directory: 'C:\\Projects\\admin-dashboard',
      risk: 'low',
      status: 'pending',
      requestedAt: now - 40_000,
      expiresAt: now + 3 * MIN,
      context: 'Step 3 of 5: creating the extracted InvoiceService with tests alongside.',
    },
    {
      id: 'p3',
      sessionId: 's3',
      agentId: 'a3',
      hostId: 'h2',
      action: 'network',
      title: 'Open network connection',
      command: 'GET https://registry.npmjs.org/vitest',
      args: [],
      directory: 'srv/api-gateway',
      risk: 'high',
      status: 'pending',
      requestedAt: now - 20_000,
      expiresAt: now + 2 * MIN,
      context: 'Resolving a pinned test-runner version from the public registry.',
    },
  ];

  const activity: ActivityEvent[] = [
    { id: 'ev1', ts: now - 8_000, kind: 'session', text: 'Codex resumed the regression suite on edge-prod-02', refId: 's3' },
    { id: 'ev2', ts: now - 20_000, kind: 'session', text: 'Claude Code passed type check in admin-dashboard', refId: 's1' },
    { id: 'ev3', ts: now - 40_000, kind: 'permission', text: 'Claude Code requested to write src/billing/invoice.ts', refId: 'p2' },
    { id: 'ev4', ts: now - 60_000, kind: 'permission', text: 'OpenCode requested to run “npm install”', refId: 'p1' },
    { id: 'ev5', ts: now - 120_000, kind: 'session', text: 'Browser Agent finished “Scrape competitor pricing”', refId: 's4' },
    { id: 'ev6', ts: now - 180_000, kind: 'agent', text: 'Codex reported busy on edge-prod-02', refId: 'a3' },
    { id: 'ev7', ts: now - 42 * MIN, kind: 'host', text: 'lab-nas went offline', refId: 'h3' },
    { id: 'ev8', ts: now - 60 * MIN, kind: 'host', text: 'edge-prod-02 connected', refId: 'h2' },
  ];

  return { hosts, agents, sessions, permissions, activity };
}
