/**
 * Adapter conformance suite (PRD §22.3). `runAdapterConformance` enforces
 * the base contract; `runSafetyConformance` enforces the G4 safety rules
 * and requires the factory to honor AdapterFaultOptions. Both register
 * node:test cases — Node-only by design (the companion runs adapters).
 *
 * Real adapters (Phase 3+) run the same suites against their factories.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { applyCommandLifecycle, isValidCapability, type CommandTracker } from '@chernobyl/domain';
import type { ServerMessage } from '@chernobyl/protocol';
import { AdapterError, type AdapterFaultOptions, type AgentAdapter } from './adapter';

type CommandResult = Extract<ServerMessage, { type: 'command_result' }>;
type PermissionRequested = Extract<ServerMessage, { type: 'permission_requested' }>;
type PermissionResolved = Extract<ServerMessage, { type: 'permission_resolved' }>;

export type AdapterFactory = (opts?: AdapterFaultOptions) => AgentAdapter;

function waitFor(
  events: ServerMessage[],
  pred: (m: ServerMessage) => boolean,
  timeoutMs = 5000,
): Promise<ServerMessage> {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const iv = setInterval(() => {
      const hit = events.find(pred);
      if (hit) {
        clearInterval(iv);
        resolve(hit);
      } else if (Date.now() - started > timeoutMs) {
        clearInterval(iv);
        reject(new Error('timed out waiting for an adapter event'));
      }
    }, 10);
  });
}

async function usableAgentId(a: AgentAdapter): Promise<string> {
  const agent = (await a.listAgents()).find((x) => x.acceptsNewSession);
  assert.ok(agent, 'adapter must expose at least one agent accepting sessions');
  return agent.id;
}

async function idleSessionId(a: AgentAdapter): Promise<string> {
  const s = (await a.listSessions()).find(
    (x) => x.status === 'completed' || x.status === 'interrupted' || x.status === 'idle',
  );
  assert.ok(s, 'adapter must expose at least one non-busy session');
  return s.id;
}

/* ------------------------------------------------------------------ */
/* Base contract                                                       */
/* ------------------------------------------------------------------ */

export function runAdapterConformance(name: string, factory: AdapterFactory): void {
  test(`[${name}] capabilities are structurally valid and honest`, async () => {
    const a = factory();
    try {
      const caps = a.capabilities();
      assert.ok(caps.length > 0);
      assert.equal(new Set(caps.map((c) => c.id)).size, caps.length, 'capability ids must be unique');
      for (const c of caps) assert.ok(isValidCapability(c), `invalid capability '${c.id}'`);
      assert.ok(
        caps.some((c) => c.status !== 'available'),
        'unsupported capabilities must be declared explicitly, not omitted',
      );
    } finally {
      await a.dispose();
    }
  });

  test(`[${name}] host-level commands fail typed as unsupported; adapter stays usable`, async () => {
    const a = factory();
    try {
      await assert.rejects(
        a.send({ type: 'ping', commandId: 'conf-ping' }),
        (e: unknown) => e instanceof AdapterError && e.code === 'unsupported',
      );
      assert.equal((await a.health()).ok, true);
    } finally {
      await a.dispose();
    }
  });

  test(`[${name}] G1: acceptance and outcome are distinct; lifecycle passes the FSM`, async () => {
    const a = factory();
    const events: ServerMessage[] = [];
    a.subscribe((m) => events.push(m));
    try {
      const ack = await a.send({
        type: 'create_session',
        commandId: 'conf-create',
        agentId: await usableAgentId(a),
        title: 'Conformance run',
        prompt: 'go',
      });
      // acceptance: the ack carries no outcome
      assert.ok(ack && ack.type === 'ack' && ack.ok === true);

      // outcome: exactly one terminal, delivered on the event channel
      const result = (await waitFor(
        events,
        (m) => m.type === 'command_result' && m.commandId === 'conf-create',
      )) as CommandResult;
      assert.equal(result.status, 'completed');

      // the recorded lifecycle validates against the domain FSM
      let tracker: CommandTracker = {};
      for (const m of events) {
        if (m.type === 'command_progress' || m.type === 'command_result') {
          tracker = applyCommandLifecycle(tracker, m);
        }
      }
      assert.equal(tracker['conf-create']?.state, 'completed');

      // envelopes are strictly increasing
      const seqs = events.filter((m) => 'seq' in m).map((m) => (m as { seq: number }).seq);
      assert.ok(seqs.length > 0);
      assert.ok(seqs.every((s, i) => i === 0 || s > seqs[i - 1]), 'envelope seq went backwards');
    } finally {
      await a.dispose();
    }
  });

  test(`[${name}] entity mapping: sessions reference declared agents`, async () => {
    const a = factory();
    try {
      const ids = new Set((await a.listAgents()).map((x) => x.id));
      for (const s of await a.listSessions()) {
        assert.ok(ids.has(s.agentId), `session ${s.id} references unknown agent ${s.agentId}`);
      }
    } finally {
      await a.dispose();
    }
  });

  test(`[${name}] G2: permission records carry native id, supported decisions, revision`, async () => {
    const a = factory();
    const events: ServerMessage[] = [];
    a.subscribe((m) => events.push(m));
    try {
      if (!a.capabilities().some((c) => c.id === 'permission.review' && c.status === 'available')) {
        return; // declared absence of permission support: nothing to enforce
      }
      await a.send({
        type: 'create_session',
        commandId: 'conf-perm',
        agentId: await usableAgentId(a),
        title: 'Permission run',
        prompt: 'go',
      });
      const asked = (await waitFor(events, (m) => m.type === 'permission_requested')) as
        PermissionRequested;
      const p = asked.permission;
      // snapshot: the engine mutates the record in place on resolution
      const revision = p.revision;
      assert.ok(p.nativeRequestId, 'native request id must ride the record');
      assert.ok(p.supportedDecisions.length > 0, 'supported decisions must be declared');
      assert.ok(revision >= 1, 'a pending record starts at a positive revision');

      // stale revision → refused fail-closed, NO resolution event
      const refused = await a.send({
        type: 'permission_decision',
        commandId: 'conf-perm-stale',
        permissionId: p.id,
        decision: 'reject',
        revision: revision + 1,
      });
      assert.ok(refused && refused.type === 'ack' && refused.ok === false);
      assert.equal(
        events.some((m) => m.type === 'permission_resolved' && m.permission.id === p.id),
        false,
      );

      // fresh revision → resolved, revision bumped, native id preserved
      const accepted = await a.send({
        type: 'permission_decision',
        commandId: 'conf-perm-ok',
        permissionId: p.id,
        decision: 'reject',
        revision,
      });
      assert.ok(accepted && accepted.type === 'ack' && accepted.ok === true);
      const resolved = (await waitFor(
        events,
        (m) => m.type === 'permission_resolved' && m.permission.id === p.id,
      )) as PermissionResolved;
      assert.equal(resolved.permission.status, 'rejected');
      assert.equal(resolved.permission.revision, revision + 1);
      assert.equal(resolved.permission.nativeRequestId, p.nativeRequestId);
    } finally {
      await a.dispose();
    }
  });
}

/* ------------------------------------------------------------------ */
/* G4 safety contract                                                  */
/* ------------------------------------------------------------------ */

export function runSafetyConformance(name: string, factory: AdapterFactory): void {
  test(`[${name}] G4.1: a runtime failure after acceptance surfaces as a failed terminal`, async () => {
    const a = factory({ lateError: true });
    const events: ServerMessage[] = [];
    a.subscribe((m) => events.push(m));
    try {
      const ack = await a.send({
        type: 'submit_prompt',
        commandId: 'conf-late',
        sessionId: await idleSessionId(a),
        text: 'run with an injected async failure',
      });
      assert.ok(ack && ack.type === 'ack' && ack.ok === true, 'acceptance must succeed');
      const failed = (await waitFor(
        events,
        (m) => m.type === 'command_result' && m.commandId === 'conf-late' && m.status === 'failed',
        3000,
      )) as CommandResult;
      assert.match(failed.error ?? '', /simulated/);
    } finally {
      await a.dispose();
    }
  });

  test(`[${name}] G4.2: a non-JSON native response is rejected, never trusted`, async () => {
    const a = factory({ nativeResponse: 'html' });
    try {
      await assert.rejects(
        a.send({
          type: 'create_session',
          commandId: 'conf-html',
          agentId: 'a4',
          title: 'x',
          prompt: 'x',
        }),
        (e: unknown) => e instanceof AdapterError && e.code === 'invalid-response',
      );
      // the adapter survives a garbage response
      assert.equal((await a.health()).ok, true);
    } finally {
      await a.dispose();
    }
  });

  test(`[${name}] G4.3: refuse mode fails closed — health reports, commands do not run`, async () => {
    const a = factory({ existingInstance: 'refuse' });
    try {
      const h = await a.health();
      assert.equal(h.ok, false);
      assert.ok(h.detail, 'a not-ok health must explain itself');
      await assert.rejects(
        a.send({ type: 'create_session', commandId: 'conf-owned', agentId: 'a4', title: 'x', prompt: 'x' }),
        (e: unknown) => e instanceof AdapterError && e.code === 'runtime-unavailable',
      );
      await assert.rejects(
        a.listSessions(),
        (e: unknown) => e instanceof AdapterError && e.code === 'runtime-unavailable',
      );
    } finally {
      await a.dispose();
    }
  });

  test(`[${name}] G4.3: attach mode declares the attachment in health`, async () => {
    const a = factory({ existingInstance: 'attach' });
    try {
      const h = await a.health();
      assert.equal(h.ok, true);
      assert.match(h.detail ?? '', /attached/);
    } finally {
      await a.dispose();
    }
  });
}
