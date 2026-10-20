import assert from 'node:assert/strict';
import test from 'node:test';
import {
  MockEngine,
  parseClientMessage,
  parseServerMessage,
  type ServerMessage,
} from '@chernobyl/protocol';

type Ack = Extract<ServerMessage, { type: 'ack' }>;
type Result = Extract<ServerMessage, { type: 'command_result' }>;

function makeEngine(): { engine: MockEngine; events: ServerMessage[] } {
  const events: ServerMessage[] = [];
  const engine = new MockEngine((m) => events.push(m));
  return { engine, events };
}

function ackOf(msg: ServerMessage | undefined): Ack | undefined {
  return msg && msg.type === 'ack' ? msg : undefined;
}

test('duplicate command ids do not create duplicate sessions', () => {
  const { engine } = makeEngine();
  const before = engine.snapshot().sessions.length;
  const cmd = {
    type: 'create_session' as const,
    commandId: 'cmd-1',
    agentId: 'a4',
    title: 'Duplicate test',
    prompt: 'Say hello',
  };

  const first = ackOf(engine.handle(cmd));
  const second = ackOf(engine.handle(cmd));

  assert.equal(first?.ok, true);
  assert.equal(second?.ok, true);
  assert.equal(second?.duplicate, true);
  assert.equal(first?.sessionId, second?.sessionId);
  assert.equal(engine.snapshot().sessions.length, before + 1);
  engine.dispose();
});

test('approving a request resolves it and resumes the session', () => {
  const { engine, events } = makeEngine();
  const permission = engine.snapshot().permissions.find((p) => p.status === 'pending');
  assert.ok(permission);

  const reply = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-1',
      permissionId: permission.id,
      decision: 'approve-once',
      revision: permission.revision,
    }),
  );

  assert.equal(reply?.ok, true);
  assert.equal(permission.status, 'approved');
  assert.equal(permission.decision, 'approve-once');
  const session = engine.snapshot().sessions.find((s) => s.id === permission.sessionId);
  assert.equal(session?.status, 'running');
  assert.ok(events.some((e) => e.type === 'permission_resolved'));
  engine.dispose();
});

test('a stale permission request can no longer be decided', () => {
  const { engine } = makeEngine();
  const permission = engine.snapshot().permissions.find((p) => p.status === 'pending');
  assert.ok(permission);
  permission.expiresAt = Date.now() - 1;

  engine.tick();
  assert.equal(permission.status, 'stale');

  const reply = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-2',
      permissionId: permission.id,
      decision: 'approve-once',
      revision: permission.revision,
    }),
  );
  assert.equal(reply?.ok, false);
  assert.equal(permission.status, 'stale');
  engine.dispose();
});

test('rejecting a request interrupts its session', () => {
  const { engine } = makeEngine();
  const permission = engine.snapshot().permissions.find(
    (p) => p.status === 'pending' && p.sessionId === 's1',
  );
  assert.ok(permission);

  const reply = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-3',
      permissionId: permission.id,
      decision: 'reject',
      revision: permission.revision,
    }),
  );

  assert.equal(reply?.ok, true);
  assert.equal(permission.status, 'rejected');
  const session = engine.snapshot().sessions.find((s) => s.id === 's1');
  assert.equal(session?.status, 'interrupted');
  assert.ok(session?.output.some((l) => l.stream === 'system' && l.text.includes('Rejected')));
  engine.dispose();
});

test('malformed frames are rejected at the protocol boundary', () => {
  assert.equal(parseClientMessage('{"type":"interrupt"}'), null);
  assert.equal(parseClientMessage('not json'), null);
  assert.equal(parseClientMessage('{"type":"nope"}'), null);
  assert.ok(parseClientMessage('{"type":"ping","commandId":"c1"}'));
});

test('a freshly created session starts running its plan immediately', () => {
  const { engine } = makeEngine();
  const ack = ackOf(
    engine.handle({
      type: 'create_session',
      commandId: 'cmd-run',
      agentId: 'a4',
      title: 'Run test',
      prompt: 'Do the thing',
    }),
  );
  assert.equal(ack?.ok, true);

  const session = engine.snapshot().sessions.find((s) => s.id === ack?.sessionId);
  assert.equal(session?.status, 'running');

  // the plan is already executing — a second prompt is refused, not queued
  const second = ackOf(
    engine.handle({
      type: 'submit_prompt',
      commandId: 'cmd-run-2',
      sessionId: ack!.sessionId!,
      text: 'again',
    }),
  );
  assert.equal(second?.ok, false);
  assert.match(second?.error ?? '', /already running/);
  engine.dispose();
});

test('interrupting a waiting session expires its pending ask', () => {
  const { engine } = makeEngine();
  const snap = engine.snapshot();
  const waiting = snap.sessions.find((s) => s.status === 'waiting');
  assert.ok(waiting);
  const ask = snap.permissions.find(
    (p) => p.sessionId === waiting.id && p.status === 'pending',
  );
  assert.ok(ask);

  const ack = ackOf(
    engine.handle({ type: 'interrupt', commandId: 'cmd-int', sessionId: waiting.id }),
  );
  assert.equal(ack?.ok, true);

  const after = engine.snapshot();
  assert.equal(after.sessions.find((s) => s.id === waiting.id)?.status, 'interrupted');
  assert.equal(after.permissions.find((p) => p.id === ask.id)?.status, 'stale');
  engine.dispose();
});

/* ------------------------------------------------------------------ */
/* Phase 1 gate requirements (docs/decisions/PHASE1-GATE-REQUIREMENTS) */
/* ------------------------------------------------------------------ */

function results(events: ServerMessage[], commandId: string): Result[] {
  return events.filter(
    (e): e is Result => e.type === 'command_result' && e.commandId === commandId,
  );
}

test('G1: an interrupted prompt command gets exactly one cancelled terminal', () => {
  const { engine, events } = makeEngine();
  const ack = ackOf(
    engine.handle({ type: 'submit_prompt', commandId: 'sub-1', sessionId: 's4', text: 'go' }),
  );
  assert.equal(ack?.ok, true);
  assert.ok(
    events.some(
      (e) => e.type === 'command_progress' && e.commandId === 'sub-1' && e.state === 'running',
    ),
  );

  const int = ackOf(engine.handle({ type: 'interrupt', commandId: 'int-1', sessionId: 's4' }));
  assert.equal(int?.ok, true);

  assert.deepEqual(results(events, 'sub-1').map((e) => e.status), ['cancelled']);
  assert.deepEqual(results(events, 'int-1').map((e) => e.status), ['completed']);
  engine.dispose();
});

test('G1: a permission lapsing mid-run yields an expired terminal, not silence', async () => {
  const { engine, events } = makeEngine();
  ackOf(engine.handle({ type: 'submit_prompt', commandId: 'sub-2', sessionId: 's4', text: 'run' }));

  // wait for the plan to reach its permission checkpoint (~1.4s of steps)
  const asked = await new Promise<Extract<ServerMessage, { type: 'permission_requested' }>>(
    (resolve, reject) => {
      const started = Date.now();
      const iv = setInterval(() => {
        const hit = events.find((e) => e.type === 'permission_requested');
        if (hit) {
          clearInterval(iv);
          resolve(hit);
        } else if (Date.now() - started > 5000) {
          clearInterval(iv);
          reject(new Error('run never reached its permission checkpoint'));
        }
      }, 20);
    },
  );

  const permission = engine.snapshot().permissions.find((p) => p.id === asked.permission.id);
  assert.ok(permission);
  permission.expiresAt = Date.now() - 1;
  engine.tick();

  assert.deepEqual(results(events, 'sub-2').map((e) => e.status), ['expired']);
  assert.equal(permission.status, 'stale');
  engine.dispose();
});

test('G1: create_session completes at acceptance — the run itself is session-level', () => {
  const { engine, events } = makeEngine();
  ackOf(
    engine.handle({
      type: 'create_session',
      commandId: 'cc-1',
      agentId: 'a4',
      title: 'Lifecycle',
      prompt: 'x',
    }),
  );
  // accepted and completed immediately; when the plan later finishes there
  // must be no SECOND terminal for the same command
  assert.deepEqual(results(events, 'cc-1').map((e) => e.status), ['completed']);
  engine.dispose();
});

test('G2: decisions fail closed on revision and on unsupported decisions', () => {
  const { engine } = makeEngine();
  const permission = engine.snapshot().permissions.find((p) => p.status === 'pending');
  assert.ok(permission);

  // wrong revision → refused, request untouched
  const stale = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-rev',
      permissionId: permission.id,
      decision: 'approve-once',
      revision: permission.revision + 1,
    }),
  );
  assert.equal(stale?.ok, false);
  assert.equal(permission.status, 'pending');

  // a decision the runtime does not offer → refused, never translated
  permission.supportedDecisions = ['reject'];
  const unsupported = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-sup',
      permissionId: permission.id,
      decision: 'approve-once',
      revision: permission.revision,
    }),
  );
  assert.equal(unsupported?.ok, false);
  assert.equal(permission.status, 'pending');

  // the supported decision at the fresh revision applies and bumps it
  const before = permission.revision;
  const ok = ackOf(
    engine.handle({
      type: 'permission_decision',
      commandId: 'd-ok',
      permissionId: permission.id,
      decision: 'reject',
      revision: permission.revision,
    }),
  );
  assert.equal(ok?.ok, true);
  assert.equal(permission.status, 'rejected');
  assert.ok(permission.revision > before);
  engine.dispose();
});

test('boundary validation rejects stale-shaped v2 messages', () => {
  // G2: a decision without a revision cannot even parse
  assert.equal(
    parseClientMessage(
      '{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"reject"}',
    ),
    null,
  );
  assert.ok(
    parseClientMessage(
      '{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"reject","revision":1}',
    ),
  );

  // G1: unknown outcomes and unenveloped events are rejected
  assert.equal(
    parseServerMessage(
      '{"type":"command_result","commandId":"c","status":"meh","seq":1,"eventId":"e","occurredAt":1}',
    ),
    null,
  );
  assert.equal(
    parseServerMessage('{"type":"command_progress","commandId":"c","state":"accepted"}'),
    null,
  );
  assert.ok(
    parseServerMessage(
      '{"type":"command_progress","commandId":"c","state":"accepted","seq":1,"eventId":"e","occurredAt":1}',
    ),
  );
  // hello_ack without lastSeq (a v1 shape) is rejected
  assert.equal(
    parseServerMessage(
      `{"type":"hello_ack","protocol":2,"serverTime":1,"snapshot":{"hosts":[],"agents":[],"sessions":[],"permissions":[],"activity":[]}}`,
    ),
    null,
  );
});
